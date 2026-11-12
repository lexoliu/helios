//! The identifiers a shard picks for the flows and questions it starts.
//!
//! An off-path sender who can guess a flow's ephemeral port or a
//! resolver question's id can forge answers to it, so neither is handed
//! out in order: each is drawn from the kernel's root entropy generator.
//!
//! Concurrency contract: every value here is owned by one shard and is
//! touched only under that shard's lock. Nothing is shared between
//! shards; two shards draw from separately derived streams.

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
/// it exactly once, from a random starting point, wrapping at the end.
///
/// This is RFC 6056 §3.3.1 (Algorithm 1, simple port randomization):
/// the start is random and the walk is linear from there, so an
/// allocation that finds the whole window in use still terminates after
/// one lap. The caller takes the first port its own suitability test
/// accepts. The walk owns no reference to the shard, so the test can
/// borrow the shard freely.
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
/// receives the flow they make.
///
/// The shard prefers a port whose flow hashes back to it, but the ports
/// that do are not evenly spaced in the window (under the standard RSS
/// key they come in runs), so walking from a random place to the next
/// such port takes the port after a long foreign run far more often
/// than the others. The pick here is made among the shard's own ports
/// instead: one pass classifies the window, and a draw `r` below the
/// window's length `W` selects the own port of rank `r * n / W` among
/// the `n` own ports, so every own port is the first candidate for
/// `floor(W / n)` or `ceil(W / n)` of the `W` draws.
///
/// Concurrency contract: a plain value, owned by the one connect that
/// built it. It is classified outside every lock, from a window and a
/// draw read under the accepting shard's lock. The own half is then
/// tried under that lock, taken again, and the foreign half afterwards,
/// each candidate under its owner's lock alone.
pub(super) struct EphemeralCandidates {
    first: u16,
    /// Ports in the window.
    window: u32,
    /// Bit `i` is set when the port `first + i` hashes to the shard.
    own: [u64; OWN_SET_WORDS],
    own_count: u32,
    /// Window offset of the own port of the selected rank; meaningful
    /// only when `own_count` is not zero.
    own_start: u32,
    /// The foreign ports are visited from the place the draw names.
    foreign: EphemeralWalk,
}

impl EphemeralCandidates {
    /// Classifies the window `first..=last` with `is_own`, and selects
    /// the starting points from `draw`, which is below the window's
    /// length.
    pub(super) fn classify(
        first: u16,
        last: u16,
        draw: u32,
        mut is_own: impl FnMut(u16) -> bool,
    ) -> Self {
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
        let mut candidates = Self {
            first,
            window,
            own,
            own_count,
            own_start: 0,
            foreign: EphemeralWalk::starting_at(first, last, draw),
        };
        if own_count != 0 {
            // `draw < window`, so the rank is below `own_count`.
            let rank = (u64::from(draw) * u64::from(own_count) / u64::from(window)) as u32;
            candidates.own_start = candidates.offset_of_rank(rank);
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

    /// The shard's own ports, each once: from the selected rank in
    /// ascending order, wrapping at the end of the window.
    pub(super) fn own_ports(&self) -> impl Iterator<Item = u16> + '_ {
        let mut from = self.own_start;
        (0..self.own_count).map(move |_| {
            let offset = self.next_own_offset(from);
            from = if offset + 1 == self.window {
                0
            } else {
                offset + 1
            };
            self.first + offset as u16
        })
    }

    /// The ports that hash to another shard, each once, from the place
    /// the draw names and wrapping at the end of the window.
    pub(super) fn foreign_ports(&self) -> impl Iterator<Item = u16> + '_ {
        self.foreign.filter(|port| !self.is_own(*port))
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
    fn a_walk_over_one_port_yields_it_once() {
        let ports: alloc::vec::Vec<u16> = EphemeralWalk::starting_at(7, 7, 0).collect();
        assert_eq!(ports, [7]);
    }
}
