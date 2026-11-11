//! The open-addressing index from a socket's key to its slab slot.
//!
//! One structure serves the TCP endpoint and listener indexes and the UDP
//! binding index. It is a fixed-capacity table with linear probing whose
//! home slot is a keyed hash of the key, and whose removal is backward-shift
//! deletion, so it has no tombstones.
//!
//! # Hash
//!
//! The home slot is SipHash-1-3 under a key derived from the per-boot
//! [`StackSecret`](crate::StackSecret), so a remote sender, which picks the
//! address and port of its own end of a connection, cannot compute which
//! tuples probe from one slot and so cannot build one long cluster. The
//! lookup runs for every received segment, which is why the cheaper
//! 1-3 round count is used rather than the 2-4 of the initial sequence
//! number function: SipHash-1-3 is the function Rust's standard `HashMap`
//! (`DefaultHasher`) rests its resistance to hash flooding on, and the
//! table's capacity is bounded, so a cluster cannot grow past it whatever
//! the hash. The key's fields are written to the hasher directly.
//!
//! # Removal
//!
//! Removing an entry shifts back the entries that follow it in its cluster
//! (Knuth, TAOCP vol. 3, §6.4, Algorithm R): one forward scan to the end of
//! the cluster, each entry examined once and moved at most once, so a
//! removal costs no more than the cluster it shortens.
//!
//! Concurrency contract: owned by one task, no interior mutability, no
//! locking; the caller serialises access.

use core::hash::Hasher;

use siphasher::sip::SipHasher13;

use crate::secret::SipKey;

/// A key the index can hash by feeding its fields to a [`Hasher`].
pub(crate) trait IndexKey: Copy + Eq {
    /// Writes every field that distinguishes this key. Two keys that are not
    /// equal write different byte streams.
    fn hash_into<H: Hasher>(self, hasher: &mut H);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry<Key> {
    key: Key,
    socket_index: usize,
    /// The slot this key's probe sequence starts at, kept so a removal does
    /// not hash the entries it shifts.
    home: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct SocketIndex<Key: IndexKey, const SLOTS: usize> {
    hash_key: SipKey,
    entries: [Option<Entry<Key>>; SLOTS],
}

impl<Key: IndexKey, const SLOTS: usize> SocketIndex<Key, SLOTS> {
    pub(crate) fn new(hash_key: SipKey) -> Self {
        assert!(SLOTS != 0, "socket index capacity must be non-zero");
        Self {
            hash_key,
            entries: [None; SLOTS],
        }
    }

    /// The slot `key`'s probe sequence starts at.
    pub(crate) fn home_slot(&self, key: Key) -> usize {
        let mut hasher = SipHasher13::new_with_keys(self.hash_key.k0, self.hash_key.k1);
        key.hash_into(&mut hasher);
        hasher.finish() as usize % SLOTS
    }

    pub(crate) fn insert(&mut self, key: Key, socket_index: usize) {
        let home = self.home_slot(key);
        for index in Self::probe(home) {
            match self.entries[index] {
                Some(entry) if entry.key == key => {
                    panic!("socket index duplicate key");
                }
                Some(_) => {}
                None => {
                    self.entries[index] = Some(Entry {
                        key,
                        socket_index,
                        home,
                    });
                    return;
                }
            }
        }
        panic!("socket index is full");
    }

    pub(crate) fn remove(&mut self, key: Key) {
        if let Some(slot) = self.find(key) {
            self.remove_at(slot);
        }
    }

    pub(crate) fn lookup(&self, key: Key) -> Option<usize> {
        self.find(key).map(|slot| self.occupied(slot).socket_index)
    }

    /// The slot holding `key`.
    pub(crate) fn find(&self, key: Key) -> Option<usize> {
        for index in Self::probe(self.home_slot(key)) {
            match self.entries[index] {
                Some(entry) if entry.key == key => return Some(index),
                Some(_) => {}
                None => return None,
            }
        }
        None
    }

    /// Empties `hole` and shifts back the entries after it that probe across
    /// it, returning how many entries moved.
    ///
    /// Algorithm R: scanning forward from the hole, an entry whose home slot
    /// does not lie in the cyclic interval `(hole, scan]` would be cut off
    /// from its home by the hole, so it moves into the hole and its old slot
    /// becomes the new hole. The scan ends at the first empty slot, the end
    /// of the cluster, so each entry is examined once and moved at most once.
    pub(crate) fn remove_at(&mut self, mut hole: usize) -> usize {
        self.entries[hole] = None;
        let mut moved = 0;
        let mut scan = Self::next(hole);
        while let Some(entry) = self.entries[scan] {
            // Cyclic distance from the entry's home to where it sits, against
            // that of the hole: the hole lies on the entry's probe path
            // exactly when the entry is at least as far from home.
            if Self::forward_distance(entry.home, scan) >= Self::forward_distance(hole, scan) {
                self.entries[hole] = Some(entry);
                self.entries[scan] = None;
                hole = scan;
                moved += 1;
            }
            scan = Self::next(scan);
        }
        moved
    }

    fn occupied(&self, slot: usize) -> Entry<Key> {
        self.entries[slot].expect("socket index slot is unoccupied")
    }

    fn probe(home: usize) -> impl Iterator<Item = usize> {
        (0..SLOTS).map(move |offset| (home + offset) % SLOTS)
    }

    const fn next(slot: usize) -> usize {
        (slot + 1) % SLOTS
    }

    /// Slots walked forward, wrapping, from `from` to reach `to`.
    const fn forward_distance(from: usize, to: usize) -> usize {
        (to + SLOTS - from) % SLOTS
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;

    use super::*;
    use crate::secret::{KeyDomain, StackSecret};

    const SLOTS: usize = 64;

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct TestKey(u32);

    impl IndexKey for TestKey {
        fn hash_into<H: Hasher>(self, hasher: &mut H) {
            hasher.write_u32(self.0);
        }
    }

    type TestIndex = SocketIndex<TestKey, SLOTS>;

    fn index() -> TestIndex {
        let secret = StackSecret::from_bytes([0x3c; StackSecret::BYTES]);
        TestIndex::new(secret.derive(KeyDomain::SocketIndex))
    }

    /// `count` distinct keys that all start probing at `home`.
    fn keys_homed_at(index: &TestIndex, home: usize, count: usize) -> Vec<TestKey> {
        (0..u32::MAX)
            .map(TestKey)
            .filter(|key| index.home_slot(*key) == home)
            .take(count)
            .collect()
    }

    /// Every entry is reachable from its home without crossing a gap, which
    /// is the invariant a lookup relies on.
    fn assert_probe_paths_unbroken(index: &TestIndex) {
        for (slot, entry) in index.entries.iter().enumerate() {
            let Some(entry) = entry else { continue };
            assert_eq!(index.home_slot(entry.key), entry.home);
            let mut walk = entry.home;
            while walk != slot {
                assert!(
                    index.entries[walk].is_some(),
                    "slot {walk} is a gap between {:?}'s home {} and its slot {slot}",
                    entry.key,
                    entry.home,
                );
                walk = (walk + 1) % SLOTS;
            }
        }
    }

    /// A fixed-seed xorshift64* generator, so a failure replays.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    #[test]
    fn a_lookup_finds_what_was_inserted_and_nothing_else() {
        let mut index = index();
        index.insert(TestKey(1), 10);
        index.insert(TestKey(2), 20);
        assert_eq!(index.lookup(TestKey(1)), Some(10));
        assert_eq!(index.lookup(TestKey(2)), Some(20));
        assert_eq!(index.lookup(TestKey(3)), None);
        index.remove(TestKey(1));
        assert_eq!(index.lookup(TestKey(1)), None);
        assert_eq!(index.lookup(TestKey(2)), Some(20));
        index.remove(TestKey(3));
    }

    #[test]
    #[should_panic(expected = "socket index duplicate key")]
    fn inserting_a_key_twice_panics() {
        let mut index = index();
        index.insert(TestKey(1), 10);
        index.insert(TestKey(1), 11);
    }

    #[test]
    #[should_panic(expected = "socket index is full")]
    fn inserting_into_a_full_table_panics() {
        let mut index = index();
        for key in 0..=SLOTS as u32 {
            index.insert(TestKey(key), 0);
        }
    }

    /// Removing the head of a cluster moves every later entry of that cluster
    /// once, and a cluster that crosses the end of the table is handled like
    /// any other.
    #[test]
    fn removing_the_head_of_a_wrapping_cluster_moves_each_entry_once() {
        let mut index = index();
        let home = SLOTS - 2;
        let keys = keys_homed_at(&index, home, 6);
        for (position, key) in keys.iter().enumerate() {
            index.insert(*key, position);
        }
        // The cluster occupies SLOTS-2, SLOTS-1, 0, 1, 2, 3.
        let slots: Vec<usize> = keys.iter().map(|key| index.find(*key).unwrap()).collect();
        assert_eq!(slots, [SLOTS - 2, SLOTS - 1, 0, 1, 2, 3]);

        let moved = index.remove_at(slots[0]);
        assert_eq!(moved, keys.len() - 1);
        assert_eq!(index.lookup(keys[0]), None);
        for (position, key) in keys.iter().enumerate().skip(1) {
            assert_eq!(index.lookup(*key), Some(position));
            assert_eq!(index.find(*key), Some(slots[position - 1]));
        }
        assert_probe_paths_unbroken(&index);
    }

    /// An entry sitting in its own home slot is never disturbed, even inside
    /// the cluster of an earlier home.
    #[test]
    fn removal_leaves_an_entry_that_is_at_home() {
        let mut index = index();
        let first = keys_homed_at(&index, 10, 3);
        let at_home = keys_homed_at(&index, 13, 1)[0];
        for (position, key) in first.iter().enumerate() {
            index.insert(*key, position);
        }
        index.insert(at_home, 99);
        assert_eq!(index.find(at_home), Some(13));
        let moved = index.remove_at(index.find(first[0]).unwrap());
        assert_eq!(moved, 2);
        assert_eq!(index.find(at_home), Some(13));
        assert_probe_paths_unbroken(&index);
    }

    #[test]
    fn a_long_random_sequence_agrees_with_a_model() {
        let mut index = index();
        let mut model: BTreeMap<TestKey, usize> = BTreeMap::new();
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        // Keys are drawn from a domain larger than the table but the load is
        // capped at three quarters, so clusters form, wrap and merge.
        let domain = 3 * SLOTS as u64;
        let capacity = 3 * SLOTS / 4;
        let mut removals_that_moved = 0;
        for step in 0..200_000 {
            let key = TestKey(rng.below(domain) as u32);
            match model.get(&key) {
                Some(_) => {
                    let slot = index.find(key).expect("model key is in the index");
                    let before: Vec<_> = index.entries.to_vec();
                    let moved = index.remove_at(slot);
                    model.remove(&key);
                    // Entries that changed slot, counted from outside, equal
                    // the moves reported: none moved twice.
                    let changed = (0..SLOTS)
                        .filter(|&s| index.entries[s].is_some() && before[s] != index.entries[s])
                        .count();
                    assert_eq!(moved, changed, "step {step}");
                    removals_that_moved += usize::from(moved != 0);
                }
                None if model.len() < capacity => {
                    index.insert(key, step);
                    model.insert(key, step);
                }
                None => continue,
            }
            if step % 97 == 0 {
                assert_probe_paths_unbroken(&index);
            }
            if step % 13 == 0 {
                for candidate in 0..domain as u32 {
                    let candidate = TestKey(candidate);
                    assert_eq!(
                        index.lookup(candidate),
                        model.get(&candidate).copied(),
                        "step {step}, key {candidate:?}",
                    );
                }
            }
        }
        assert!(
            removals_that_moved > 1_000,
            "the sequence never exercised a shift"
        );
    }
}
