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

use super::{EPHEMERAL_PORT_END, EPHEMERAL_PORT_START};
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
/// outbound TCP connect walks its foreign candidates from a start
/// [`ConnectPortSelector`] names, RFC 6056 §3.3.3.
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

/// Where the candidates of an outbound TCP connect start: RFC 6056
/// §3.3.3 (Algorithm 3, simple hash-based port selection), the scheme
/// Linux's `__inet_hash_connect` uses, taken in the rank space of the
/// shard's own ports ([`EphemeralCandidates`]).
///
/// A connection to `(local address, remote address, remote port)` starts
/// at `F(those, key) + counter`, where `F` is SipHash-2-4, the keyed hash
/// [`helios_netstack::initial_sequence_number`] also uses, and the
/// counter belongs to the destination and advances with every connect to
/// it. A uniformly random start per connect (Algorithm 1) makes every
/// connect an independent draw, so the same port returns within a few
/// connects, and a peer that closed the previous connection first still
/// holds that four-tuple and resets the new SYN. Here successive connects
/// to one destination take successive candidates, and a four-tuple comes
/// back only after a lap of them.
///
/// The ports are taken in two kinds, so a destination has two counters
/// ([`ConnectCounters`]): one over the shard's own ports in rank order,
/// which a connect takes while any is free, and one over the window's
/// positions, which orders the ports that hash to another shard once
/// every own port is in use. Each advances only for a take of its own
/// kind, past the port taken.
///
/// Unpredictability, as in RFC 6056 §3.3.3: an off-path attacker who
/// does not hold the 128-bit key, drawn once per shard from the root
/// entropy generator, cannot compute `F`, so the first port of a
/// destination is unpredictable to it; later ports follow the first by
/// the observable count of connects, as Algorithm 3 accepts.
///
/// One pair of counters per destination would need a table keyed by
/// destination; [`Self::SLOTS`] pairs indexed by the hash stand in for
/// it, as Linux's perturbation table does. Two destinations that share a
/// slot only advance each other's counters faster, which shortens the
/// lap and never repeats a port sooner than the lap allows for the sum
/// of their connects.
///
/// Concurrency contract: owned by one shard, read and written only under
/// its lock; the key is written once at construction.
pub(super) struct ConnectPortSelector {
    key: [u8; Self::KEY_BYTES],
    counters: [ConnectCounters; Self::SLOTS],
}

/// How far the connects to one destination have advanced its start.
///
/// Both counts are kept unreduced and reduced only when read: the own
/// count by the number of own ports of the destination reading it, which
/// differs between two destinations that share a slot, and the foreign
/// count by the window's length. Reducing a shared counter by one
/// destination's own-port count would move another destination's start
/// backwards; an unreduced sum only ever moves each forward. A connect
/// advances one count by at most the window's length, no more than the
/// 2^14 ports of the ephemeral range, so a count wraps, and the start it
/// names jumps once, after no fewer than 2^50 connects through one slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ConnectCounters {
    /// Own ports passed, in rank order.
    own: u64,
    /// Window positions passed by foreign takes.
    foreign: u64,
}

/// The counters a destination hashed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConnectSlot(usize);

/// What one connect read from its shard to place its candidates: the
/// window, the keyed start `F` reduced into it, and the destination's
/// counters at that moment.
///
/// Concurrency contract: a plain value, read under the accepting shard's
/// lock and carried out of it; [`EphemeralCandidates::classify`] places
/// the candidates from it outside every lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ConnectStart {
    first: u16,
    last: u16,
    /// `F`, below the window's length.
    keyed: u32,
    counters: ConnectCounters,
}

impl ConnectStart {
    /// The start over `first..=last` at the keyed position `keyed`,
    /// below the window's length, with the destination's `counters`.
    pub(super) fn new(first: u16, last: u16, keyed: u32, counters: ConnectCounters) -> Self {
        debug_assert!(first <= last, "an ephemeral window is never empty");
        debug_assert!(
            keyed < EphemeralWalk::window_len(first, last),
            "the keyed start lies inside the window"
        );
        Self {
            first,
            last,
            keyed,
            counters,
        }
    }
}

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
            counters: [ConnectCounters::default(); Self::SLOTS],
        }
    }

    /// The start over `first..=last` for a connection from `local` to
    /// `destination:port`, and the counters its take advances.
    pub(super) fn start(
        &self,
        first: u16,
        last: u16,
        local: IpAddress,
        destination: IpAddress,
        port: u16,
    ) -> (ConnectStart, ConnectSlot) {
        let mut hasher = SipHasher24::new_with_key(&self.key);
        write_address(&mut hasher, local);
        write_address(&mut hasher, destination);
        hasher.write_u16(port);
        let hash = hasher.finish();
        let slot = ConnectSlot((hash >> 32) as usize % Self::SLOTS);
        // `F` is the low 32 bits of the digest; the slot uses the high
        // ones, so the two are independent.
        let keyed = (hash as u32) % EphemeralWalk::window_len(first, last);
        (
            ConnectStart::new(first, last, keyed, self.counters[slot.0]),
            slot,
        )
    }

    /// Records that a connect took the own port `taken`: the
    /// destination's next connect starts at the own port ranked after
    /// it.
    ///
    /// Advancing by one would restart the next connect at the rank after
    /// this one's start, and when the taken port lies ranks into the
    /// candidates (the ones before it are held) the next connect would
    /// take that same port again. The advance is additive, so a connect
    /// that raced this one on the same destination keeps its own
    /// advance. Counted in ranks from the start, the own ports one
    /// destination takes increase strictly until the count has gone a
    /// whole lap of the own ports, which is the no-repeat bound.
    pub(super) fn took_own(&mut self, slot: ConnectSlot, taken: OwnCandidate) {
        let counters = &mut self.counters[slot.0];
        counters.own = counters.own.wrapping_add(u64::from(taken.ranks) + 1);
    }

    /// Records that a connect, every own port being in use, took the
    /// foreign port `taken`: the destination's next foreign candidates
    /// start at the window position after it.
    ///
    /// The own count is left where it is. Every own port was tried and
    /// found in use, so the next own port to try is still the one ranked
    /// after the last own port taken, the one taken longest ago; moving
    /// the own count here would pass over own ports out of their turn.
    /// The foreign count follows the rule of [`Self::took_own`] in window
    /// positions, so the foreign ports one destination takes increase
    /// strictly until that count has gone a whole lap of the window.
    pub(super) fn took_foreign(&mut self, slot: ConnectSlot, taken: ForeignCandidate) {
        let counters = &mut self.counters[slot.0];
        counters.foreign = counters.foreign.wrapping_add(u64::from(taken.steps) + 1);
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

/// Ports in the whole ephemeral range, the largest window a shard can
/// have (one shard owns all of it).
const EPHEMERAL_RANGE_PORTS: usize = (EPHEMERAL_PORT_END - EPHEMERAL_PORT_START) as usize + 1;

/// 64-bit words a set over [`EPHEMERAL_RANGE_PORTS`] ports takes.
const OWN_SET_WORDS: usize = EPHEMERAL_RANGE_PORTS.div_ceil(u64::BITS as usize);

/// The ports of a shard's window a connect may try, sorted by who
/// receives the flow they make, in the order one [`ConnectStart`] names.
///
/// The shard prefers a port whose flow hashes back to it, but the ports
/// that do are not evenly spaced in the window (under the standard RSS
/// key they come in runs), so walking from a uniform place in the window
/// to the next such port takes the port after a long foreign run far
/// more often than the others. The walk here runs over the shard's own
/// ports in rank order instead: one pass classifies the window into its
/// `n` own ports of `W`, the keyed start `F < W` maps to the rank
/// `F * n / W`, and the destination's own count is added to it modulo
/// `n`. With the count at zero every own port is the first candidate for
/// `floor(W / n)` or `ceil(W / n)` of the `W` keyed starts, and because
/// the count advances in rank space past the port taken
/// ([`ConnectPortSelector::took_own`]), successive connects to one
/// destination take successive own ports and none returns before a lap
/// of them. Advancing in window positions instead would map several
/// consecutive counts to one rank, and take one port again.
///
/// The ports that hash to another shard are tried only once every own
/// port is in use, in window order from `F` plus the destination's
/// foreign count, as RFC 6056 §3.3.3 walks a window
/// ([`ConnectPortSelector::took_foreign`]).
///
/// Concurrency contract: a plain value, owned by the one connect that
/// built it. It is classified outside every lock, from a
/// [`ConnectStart`] read under the accepting shard's lock. The own half
/// is then tried under that lock, taken again, and the foreign half
/// afterwards, each candidate under its owner's lock alone.
pub(super) struct EphemeralCandidates {
    first: u16,
    /// Ports in the window.
    window: u32,
    /// Bit `i` is set when the port `first + i` hashes to the shard.
    own: [u64; OWN_SET_WORDS],
    own_count: u32,
    /// Window offset of the own port of the start rank; meaningful only
    /// when `own_count` is not zero.
    own_start: u32,
    /// The foreign ports are visited from the place the keyed start and
    /// the foreign count name.
    foreign: EphemeralWalk,
}

/// An own port a connect may take, with the ranks of own ports before
/// it among the candidates, which [`ConnectPortSelector::took_own`]
/// advances past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OwnCandidate {
    pub(super) port: u16,
    ranks: u32,
}

/// A port that hashes to another shard, with the window positions
/// before it among the foreign candidates' walk, which
/// [`ConnectPortSelector::took_foreign`] advances past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ForeignCandidate {
    pub(super) port: u16,
    steps: u32,
}

impl EphemeralCandidates {
    /// Classifies the window of `start` with `is_own`, and places both
    /// halves' first candidates from it.
    pub(super) fn classify(start: ConnectStart, mut is_own: impl FnMut(u16) -> bool) -> Self {
        let ConnectStart {
            first,
            last,
            keyed,
            counters,
        } = start;
        let window = EphemeralWalk::window_len(first, last);
        debug_assert!(
            window as usize <= EPHEMERAL_RANGE_PORTS,
            "a window lies inside the ephemeral range"
        );
        let mut own = [0u64; OWN_SET_WORDS];
        let mut own_count = 0u32;
        for offset in 0..window {
            // `offset < window <= u16::MAX + 1`, and `first + offset <= last`.
            if is_own(first + offset as u16) {
                own[(offset / u64::BITS) as usize] |= 1 << (offset % u64::BITS);
                own_count += 1;
            }
        }
        // Both terms are below `window`, so the sum fits and the offset
        // is below `window`.
        let foreign_offset =
            (u64::from(keyed) + counters.foreign % u64::from(window)) % u64::from(window);
        let mut candidates = Self {
            first,
            window,
            own,
            own_count,
            own_start: 0,
            foreign: EphemeralWalk::starting_at(first, last, foreign_offset as u32),
        };
        if own_count != 0 {
            let own_count = u64::from(own_count);
            // `keyed < window`, so the keyed rank is below `own_count`.
            let keyed_rank = u64::from(keyed) * own_count / u64::from(window);
            let rank = (keyed_rank + counters.own % own_count) % own_count;
            candidates.own_start = candidates.offset_of_rank(rank as u32);
        }
        candidates
    }

    /// The window offset of the own port with `rank` among the own
    /// ports in ascending order, with `rank` below `own_count`.
    fn offset_of_rank(&self, mut rank: u32) -> u32 {
        for (index, word) in self.own.iter().enumerate() {
            let in_word = word.count_ones();
            if rank < in_word {
                let mut bits = *word;
                for _ in 0..rank {
                    // Clears the lowest set bit.
                    bits &= bits - 1;
                }
                return index as u32 * u64::BITS + bits.trailing_zeros();
            }
            rank -= in_word;
        }
        unreachable!("the rank is below the own count");
    }

    /// The first own port at window offset `from` or after it, wrapping
    /// to the start of the window. Requires an own port to exist.
    fn next_own_offset(&self, from: u32) -> u32 {
        let mut index = (from / u64::BITS) as usize;
        let mut bits = self.own[index] & (!0u64 << (from % u64::BITS));
        loop {
            if bits != 0 {
                return index as u32 * u64::BITS + bits.trailing_zeros();
            }
            index = (index + 1) % OWN_SET_WORDS;
            bits = self.own[index];
        }
    }

    fn is_own(&self, port: u16) -> bool {
        let offset = u32::from(port - self.first);
        self.own[(offset / u64::BITS) as usize] & (1 << (offset % u64::BITS)) != 0
    }

    /// The shard's own ports, each once: from the start rank in
    /// ascending order, wrapping at the end of the window.
    pub(super) fn own_ports(&self) -> impl Iterator<Item = OwnCandidate> + '_ {
        let mut from = self.own_start;
        (0..self.own_count).map(move |ranks| {
            let offset = self.next_own_offset(from);
            from = if offset + 1 == self.window {
                0
            } else {
                offset + 1
            };
            OwnCandidate {
                port: self.first + offset as u16,
                ranks,
            }
        })
    }

    /// The ports that hash to another shard, each once, from the place
    /// the keyed start and the foreign count name, wrapping at the end
    /// of the window.
    pub(super) fn foreign_ports(&self) -> impl Iterator<Item = ForeignCandidate> + '_ {
        self.foreign
            .filter(|port| !self.is_own(*port))
            .map(|port| ForeignCandidate {
                port,
                steps: self.foreign.steps_to(port),
            })
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

    fn test_selector() -> ConnectPortSelector {
        ConnectPortSelector {
            key: [7; ConnectPortSelector::KEY_BYTES],
            counters: [ConnectCounters::default(); ConnectPortSelector::SLOTS],
        }
    }

    fn test_address() -> IpAddress {
        IpAddress::Ipv4(helios_netstack::Ipv4Address::new([192, 0, 2, 1]))
    }

    /// The window of the lap tests, its own ports, and the own ports a
    /// connect finds in use. The held ports leave runs of in-use own
    /// ports, so a take lies ranks into its candidates.
    const LAP_WINDOW: (u16, u16) = (100, 163);
    fn lap_is_own(port: u16) -> bool {
        port % 3 == 0
    }
    fn lap_is_held(port: u16) -> bool {
        port % 9 == 0 || port % 15 == 0
    }

    /// The no-repeat bound of [`ConnectPortSelector::took_own`]: taking
    /// the first own candidate not in use and advancing past it takes
    /// every free own port of the window once before the first repeat,
    /// and the lap then starts over in the same order.
    #[test]
    fn advancing_past_the_taken_port_laps_the_own_ports_once() {
        let (first, last) = LAP_WINDOW;
        let free = (first..=last)
            .filter(|port| lap_is_own(*port) && !lap_is_held(*port))
            .count();
        let mut selector = test_selector();
        let mut taken = alloc::vec::Vec::new();
        for _ in 0..free * 2 {
            let (start, slot) = selector.start(first, last, test_address(), test_address(), 80);
            let candidates = EphemeralCandidates::classify(start, lap_is_own);
            let candidate = candidates
                .own_ports()
                .find(|candidate| !lap_is_held(candidate.port))
                .expect("an own port is free");
            selector.took_own(slot, candidate);
            taken.push(candidate.port);
        }
        let (lap, rest) = taken.split_at(free);
        let mut distinct = lap.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), free, "a port repeated inside one lap");
        assert_eq!(lap, rest, "the second lap walks the same order");
    }

    /// The no-repeat bound of [`ConnectPortSelector::took_foreign`]:
    /// with every own port in use, taking the first free foreign
    /// candidate and advancing past it takes every free foreign port of
    /// the window once before the first repeat, and leaves the own
    /// ports' start where it was.
    #[test]
    fn advancing_past_a_foreign_take_laps_the_foreign_ports_once() {
        let (first, last) = LAP_WINDOW;
        let free = (first..=last)
            .filter(|port| !lap_is_own(*port) && !lap_is_held(*port))
            .count();
        let mut selector = test_selector();
        let (start, _) = selector.start(first, last, test_address(), test_address(), 80);
        let own_start = EphemeralCandidates::classify(start, lap_is_own)
            .own_ports()
            .next()
            .expect("an own port exists");
        let mut taken = alloc::vec::Vec::new();
        for _ in 0..free * 2 {
            let (start, slot) = selector.start(first, last, test_address(), test_address(), 80);
            let candidates = EphemeralCandidates::classify(start, lap_is_own);
            assert_eq!(
                candidates.own_ports().next(),
                Some(own_start),
                "a foreign take moved the own ports' start"
            );
            let candidate = candidates
                .foreign_ports()
                .find(|candidate| !lap_is_held(candidate.port))
                .expect("a foreign port is free");
            selector.took_foreign(slot, candidate);
            taken.push(candidate.port);
        }
        let (lap, rest) = taken.split_at(free);
        let mut distinct = lap.to_vec();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), free, "a port repeated inside one lap");
        assert_eq!(lap, rest, "the second lap walks the same order");
    }

    #[test]
    fn a_walk_over_one_port_yields_it_once() {
        let ports: alloc::vec::Vec<u16> = EphemeralWalk::starting_at(7, 7, 0).collect();
        assert_eq!(ports, [7]);
    }
}
