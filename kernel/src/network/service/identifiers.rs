//! The identifiers a shard picks for the flows and questions it starts.
//!
//! An off-path sender who can guess a flow's ephemeral port or a
//! resolver question's id can forge answers to it, so neither is handed
//! out in order: each is drawn from the kernel's root entropy generator,
//! or, for an outbound TCP connection, derived from a key that
//! generator supplied ([`ConnectPortSelector`]).
//!
//! Concurrency contract: every value here is owned by one shard and is
//! touched only under that shard's lock. Nothing is shared between
//! shards; two shards draw from separately derived streams and key their
//! connect-port hashes separately.

use core::hash::Hasher;

use helios_netstack::IpAddress;
use siphasher::sip::SipHasher24;

use crate::{EntropyPool, RootEntropyHandle};

/// The random stream one shard draws its identifiers from.
///
/// The pool is derived from the root generator when the shard is built
/// and owned by the shard afterwards, which is how every other consumer
/// of the root generator takes randomness ([`EntropyPool`]); a draw
/// costs no lock on the root.
pub(super) struct ShardEntropy {
    root: RootEntropyHandle,
    pool: EntropyPool,
}

impl ShardEntropy {
    /// `shard_idx` personalizes the derivation so two shards built at
    /// the same moment get different streams.
    pub(super) fn derive(root: RootEntropyHandle, shard_idx: usize) -> Self {
        let pool = EntropyPool::derive(&root, shard_idx as u64);
        Self { root, pool }
    }

    /// A value in `0..bound` from the pool's cryptographic stream.
    ///
    /// The modulo bias of a 64-bit draw over a bound below 2^32 is below
    /// 2^-32, which no attacker can use.
    pub(super) fn below(&mut self, bound: u32) -> u32 {
        debug_assert!(bound != 0, "a draw below zero has no value");
        (self.pool.secure_u64(&self.root) % u64::from(bound)) as u32
    }

    /// A fresh 128-bit key, the width SipHash-2-4 takes.
    fn key(&mut self) -> [u8; ConnectPortSelector::KEY_BYTES] {
        let mut key = [0_u8; ConnectPortSelector::KEY_BYTES];
        for half in key.as_chunks_mut::<8>().0 {
            half.copy_from_slice(&self.pool.secure_u64(&self.root).to_le_bytes());
        }
        key
    }

    /// A uniform nonzero 16-bit identifier: a DNS question id or an ICMP
    /// echo identifier, neither of which may be zero here.
    pub(super) fn nonzero_u16(&mut self) -> u16 {
        // `below` is under `NONZERO_U16_VALUES`, so `1 +` lies in
        // `1..=u16::MAX`.
        (1 + self.below(NONZERO_U16_VALUES)) as u16
    }
}

/// How many values a 16-bit identifier may take once zero is excluded.
pub(super) const NONZERO_U16_VALUES: u32 = u16::MAX as u32;

/// One walk over a shard's window of the ephemeral range: every port in
/// it exactly once, from a given starting point, wrapping at the end.
///
/// The walk is linear from its start, so an allocation that finds the
/// whole window in use still terminates after one lap. The caller takes
/// the first port its own suitability test accepts. The walk owns no
/// reference to the shard, so the test can borrow the shard freely.
///
/// Where the start comes from is the caller's policy: a uniformly random
/// one is RFC 6056 §3.3.1 (Algorithm 1, simple port randomization), which
/// a flow with no peer uses (`NetworkShard::ephemeral_walk`); an
/// outbound TCP connect uses [`ConnectPortSelector`], RFC 6056 §3.3.3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EphemeralWalk {
    first: u16,
    last: u16,
    next: u16,
    remaining: u32,
}

impl EphemeralWalk {
    /// Ports in the inclusive window `first..=last`.
    pub(super) const fn window_len(first: u16, last: u16) -> u32 {
        (last - first) as u32 + 1
    }

    /// A walk over `first..=last` that begins `offset` ports into the
    /// window, with `offset` below [`Self::window_len`].
    pub(super) fn starting_at(first: u16, last: u16, offset: u32) -> Self {
        let len = Self::window_len(first, last);
        debug_assert!(first <= last, "an ephemeral window is never empty");
        debug_assert!(offset < len, "the start lies inside the window");
        Self {
            first,
            last,
            // `offset < len <= u16::MAX + 1`, and `first + offset <= last`.
            next: first + offset as u16,
            remaining: len,
        }
    }

    /// How many candidates of this walk, counted from its start, come
    /// before `port`: the walk reaches `port` after that many steps.
    /// `port` must lie in the window.
    pub(super) fn steps_to(&self, port: u16) -> u32 {
        debug_assert!(
            (self.first..=self.last).contains(&port),
            "the port lies inside the window"
        );
        if port >= self.next {
            u32::from(port - self.next)
        } else {
            u32::from(self.last - self.next) + 1 + u32::from(port - self.first)
        }
    }
}

/// Where the walk of an outbound TCP connect starts: RFC 6056 §3.3.3
/// (Algorithm 3, simple hash-based port selection), the scheme Linux's
/// `__inet_hash_connect` uses.
///
/// The walk for a connection to `(local address, remote address, remote
/// port)` starts at `F(those, key) + counter`, where `F` is SipHash-2-4,
/// the keyed hash [`helios_netstack::initial_sequence_number`] also uses,
/// and the counter belongs to the destination and advances with every
/// connect to it ([`Self::advance`]). A uniformly random start per
/// connect (Algorithm 1) makes every connect an independent draw, so the
/// same port returns within a few connects, and a peer that closed the
/// previous connection first still holds that four-tuple and resets the
/// new SYN. Here successive connects to one destination take successive
/// candidates of the window, and a four-tuple comes back only after the
/// whole window has been walked.
///
/// Unpredictability, as in RFC 6056 §3.3.3: an off-path attacker who
/// does not hold the 128-bit key, drawn once per shard from the root
/// entropy generator, cannot compute `F`, so the first port of a
/// destination is unpredictable to it; later ports follow the first by
/// the observable count of connects, as Algorithm 3 accepts.
///
/// One counter per destination would need a table keyed by destination;
/// [`Self::SLOTS`] counters indexed by the hash stand in for it, as
/// Linux's perturbation table does. Two destinations that share a slot
/// only advance each other's counter faster, which shortens the lap and
/// never repeats a port sooner than the window allows for the sum of
/// their connects.
///
/// Concurrency contract: owned by one shard, read and written only under
/// its lock; the key is written once at construction.
pub(super) struct ConnectPortSelector {
    key: [u8; Self::KEY_BYTES],
    /// Each counter is kept below the window length of the shard.
    counters: [u32; Self::SLOTS],
}

/// The counter a destination hashed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConnectSlot(usize);

impl ConnectPortSelector {
    /// SipHash-2-4 takes a 128-bit key.
    const KEY_BYTES: usize = 16;
    /// A power of two well above the destinations a shard connects to at
    /// once; Linux's table has 2^16 entries for a whole host, a shard
    /// sees a fraction of its flows.
    pub(super) const SLOTS: usize = 256;

    /// Draws the key from the shard's entropy stream.
    pub(super) fn new(entropy: &mut ShardEntropy) -> Self {
        Self {
            key: entropy.key(),
            counters: [0; Self::SLOTS],
        }
    }

    /// The walk over `first..=last` for a connection from `local` to
    /// `destination:port`, and the counter it advances.
    pub(super) fn walk(
        &self,
        first: u16,
        last: u16,
        local: IpAddress,
        destination: IpAddress,
        port: u16,
    ) -> (EphemeralWalk, ConnectSlot) {
        let mut hasher = SipHasher24::new_with_key(&self.key);
        write_address(&mut hasher, local);
        write_address(&mut hasher, destination);
        hasher.write_u16(port);
        let hash = hasher.finish();
        let slot = ConnectSlot((hash >> 32) as usize % Self::SLOTS);
        let len = EphemeralWalk::window_len(first, last);
        // `F` is the low 32 bits of the digest; the slot uses the high
        // ones, so the two are independent.
        let offset = ((hash as u32) % len + self.counters[slot.0]) % len;
        (EphemeralWalk::starting_at(first, last, offset), slot)
    }

    /// Records that the connect which started at `walk` took the
    /// candidate `steps` into it, whether an own-flow candidate or, once
    /// those were all in use, a foreign-shard one: the destination's next
    /// walk starts at the port after the taken one.
    ///
    /// Advancing by one would restart the next walk at the port after
    /// the walk's own start, and when the taken port lies steps into the
    /// walk (candidates that hash to another shard or are held are
    /// skipped) the next connect would take that same port again. The
    /// advance is additive, so a connect that raced this one on the
    /// same destination keeps its own advance. Counted in walk steps from
    /// the start, the taken ports of one destination increase strictly
    /// until the counter has gone a whole lap round the window, which is
    /// the no-repeat bound.
    pub(super) fn advance(&mut self, slot: ConnectSlot, walk: &EphemeralWalk, steps: u32) {
        let len = EphemeralWalk::window_len(walk.first, walk.last);
        debug_assert!(steps < len, "a step count lies inside the window");
        self.counters[slot.0] = (self.counters[slot.0] + steps + 1) % len;
    }
}

/// Feeds an address as a family tag and its octets, so an IPv4 and an
/// IPv6 address can never produce the same input.
fn write_address(hasher: &mut SipHasher24, address: IpAddress) {
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

impl Iterator for EphemeralWalk {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.remaining == 0 {
            return None;
        }
        let port = self.next;
        self.next = if port == self.last {
            self.first
        } else {
            port + 1
        };
        self.remaining -= 1;
        Some(port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walk_visits_every_port_of_its_window_once() {
        for (first, last, offset) in [
            (49_152, 49_160, 0),
            (49_152, 49_160, 5),
            (60_000, 65_535, 5_535),
        ] {
            let ports: alloc::vec::Vec<u16> =
                EphemeralWalk::starting_at(first, last, offset).collect();
            assert_eq!(ports.len() as u32, EphemeralWalk::window_len(first, last));
            assert_eq!(ports[0], first + offset as u16);
            let mut sorted = ports.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), ports.len(), "no port is visited twice");
            assert_eq!(*sorted.first().unwrap(), first);
            assert_eq!(*sorted.last().unwrap(), last);
        }
    }

    #[test]
    fn a_walk_wraps_at_the_end_of_its_window() {
        let ports: alloc::vec::Vec<u16> = EphemeralWalk::starting_at(10, 13, 2).collect();
        assert_eq!(ports, [12, 13, 10, 11]);
    }

    #[test]
    fn steps_to_counts_candidates_before_a_port_across_the_wrap() {
        let walk = EphemeralWalk::starting_at(10, 13, 2);
        let steps: alloc::vec::Vec<u32> = [12, 13, 10, 11]
            .into_iter()
            .map(|port| walk.steps_to(port))
            .collect();
        assert_eq!(steps, [0, 1, 2, 3]);
    }

    /// The no-repeat bound of [`ConnectPortSelector::advance`]: taking
    /// the first candidate that passes a filter and advancing past it
    /// visits every passing port of the window once before the first
    /// repeat, and the lap then starts over.
    #[test]
    fn advancing_past_the_taken_port_laps_the_filtered_candidates_once() {
        let (first, last) = (100_u16, 163_u16);
        let passes = |port: u16| port % 3 == 0;
        let passing = (first..=last).filter(|port| passes(*port)).count();
        let mut selector = ConnectPortSelector {
            key: [7; ConnectPortSelector::KEY_BYTES],
            counters: [0; ConnectPortSelector::SLOTS],
        };
        let address = IpAddress::Ipv4(helios_netstack::Ipv4Address::new([192, 0, 2, 1]));
        let mut taken = alloc::vec::Vec::new();
        for _ in 0..passing * 2 {
            let (walk, slot) = selector.walk(first, last, address, address, 80);
            let port = { walk }.find(|port| passes(*port)).expect("a port passes");
            selector.advance(slot, &walk, walk.steps_to(port));
            taken.push(port);
        }
        let (lap, rest) = taken.split_at(passing);
        let mut distinct = lap.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), passing, "a port repeated inside one lap");
        assert_eq!(lap, rest, "the second lap walks the same order");
    }

    #[test]
    fn a_walk_over_one_port_yields_it_once() {
        let ports: alloc::vec::Vec<u16> = EphemeralWalk::starting_at(7, 7, 0).collect();
        assert_eq!(ports, [7]);
    }
}
