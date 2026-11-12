//! The identifiers a shard picks for the flows and questions it starts.
//!
//! An off-path sender who can guess a flow's ephemeral port or a
//! resolver question's id can forge answers to it, so neither is handed
//! out in order: each is drawn from the kernel's root entropy generator.
//!
//! Concurrency contract: every value here is owned by one shard and is
//! touched only under that shard's lock. Nothing is shared between
//! shards; two shards draw from separately derived streams.

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
}

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
