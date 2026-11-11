//! Runtime objects built from a cwasm artifact, cached by artifact content.
//!
//! A lookup is keyed by the artifact's bytes, but hashing and comparing a
//! whole cwasm (tens of megabytes for coreutils) costs milliseconds, so the
//! cache answers most lookups from the artifact's *region*: the
//! `(pointer, length)` of the memory the caller's `Bytes` views. Every region
//! the cache has matched is recorded against its entry, so each distinct
//! region pays the content hash and compare at most once, and every later
//! lookup from it, from any caller, is a pointer-sized hash.
//!
//! # Why a region key cannot alias another artifact
//!
//! A region key is sound only while the memory it names holds the bytes it
//! was matched against. The cache keeps a `Bytes` clone of every recorded
//! region, so the memory cannot be freed (and its address reused for a
//! different artifact) while the key exists, and it cannot change: `Bytes`
//! is immutable, and the cache's clone keeps the reference count above one,
//! so no owner can reclaim it as `BytesMut`. A bootfs file is a
//! `Bytes::from_static` view of the boot module, which is never freed; a
//! file loaded into the heap is reference-counted.
//!
//! # Retention
//!
//! Holding a clone of a heap region would otherwise keep it alive for as
//! long as its entry stays resident, even after every outside owner has
//! dropped it (a host-share file is read into a fresh buffer on every exec).
//! Before a region is recorded, the cache drops every recorded region whose
//! clone is the last handle to its memory, so the cache never pins a region
//! past the next content lookup that follows its last outside owner going
//! away. A static region is never the last handle and stays recorded, which
//! costs nothing. An entry's own artifact is the region the entry owns and
//! is charged to the budget; recorded regions are not charged, because they
//! hold no runtime object and no memory beyond what their owners already
//! hold.
//!
//! # Concurrency
//!
//! Every operation takes `&mut self`; callers serialise them through the
//! lock that owns the cache, from whichever processor runs the spawning
//! task. No interrupt handler reaches a cache. The reference-count test that
//! releases dead regions is exact under that exclusion: when the cache's
//! clone is the only handle, no other processor holds one to clone it from.

extern crate alloc;

use alloc::sync::Arc;
use bytes::Bytes;
use core::hash::BuildHasher;
use hashbrown::{DefaultHashBuilder, HashMap, HashTable};
use lru::LruCache;

pub struct ComponentCache<Component, ContentHasher = DefaultHashBuilder> {
    budget_bytes: usize,
    resident_bytes: usize,
    next_entry: u64,
    /// Resident entries in recency order. Every hit, by region or by
    /// content, promotes its entry.
    entries: LruCache<EntryId, Entry<Component>>,
    /// Entries by the hash of their artifact bytes; a match is confirmed by
    /// comparing the bytes.
    by_content: HashTable<ContentSlot>,
    content_hasher: ContentHasher,
    /// Every region matched to a resident entry, its own artifact included.
    by_region: HashMap<RegionIdentity, Region>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct EntryId(u64);

struct Entry<Component> {
    artifact: Bytes,
    content_hash: u64,
    component: Arc<Component>,
}

struct ContentSlot {
    content_hash: u64,
    entry: EntryId,
}

struct Region {
    /// Keeps the memory behind the region key alive and immutable.
    bytes: Bytes,
    entry: EntryId,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct RegionIdentity {
    ptr: usize,
    len: usize,
}

impl RegionIdentity {
    fn of(bytes: &Bytes) -> Self {
        Self {
            ptr: bytes.as_ptr() as usize,
            len: bytes.len(),
        }
    }
}

enum Lookup {
    Hit(EntryId),
    Miss { content_hash: u64 },
}

impl<Component> ComponentCache<Component> {
    pub fn new(budget_bytes: usize) -> Self {
        Self::with_content_hasher(budget_bytes, DefaultHashBuilder::default())
    }
}

impl<Component, ContentHasher: BuildHasher> ComponentCache<Component, ContentHasher> {
    fn with_content_hasher(budget_bytes: usize, content_hasher: ContentHasher) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            next_entry: 0,
            entries: LruCache::unbounded(),
            by_content: HashTable::new(),
            content_hasher,
            by_region: HashMap::new(),
        }
    }

    pub fn get(&mut self, artifact: &Bytes) -> Option<Arc<Component>> {
        match self.lookup(artifact) {
            Lookup::Hit(entry) => Some(self.promote(entry)),
            Lookup::Miss { .. } => None,
        }
    }

    pub fn insert_if_missing(
        &mut self,
        artifact: Bytes,
        component: Arc<Component>,
    ) -> Arc<Component> {
        let content_hash = match self.lookup(&artifact) {
            Lookup::Hit(entry) => return self.promote(entry),
            Lookup::Miss { content_hash } => content_hash,
        };

        let entry = EntryId(self.next_entry);
        self.next_entry = self
            .next_entry
            .checked_add(1)
            .expect("component cache entry ids exhausted");
        self.resident_bytes = self
            .resident_bytes
            .checked_add(artifact.len())
            .expect("component cache byte accounting overflow");
        self.record_region(&artifact, entry);
        self.by_content.insert_unique(
            content_hash,
            ContentSlot {
                content_hash,
                entry,
            },
            |slot| slot.content_hash,
        );
        let displaced = self.entries.put(
            entry,
            Entry {
                artifact,
                content_hash,
                component: component.clone(),
            },
        );
        assert!(
            displaced.is_none(),
            "component cache reused a resident entry id"
        );
        self.evict_to_budget();
        component
    }

    /// Finds the entry for `artifact`, by region first and by content only
    /// when the region is new. A content hit records the region, so the
    /// next lookup from it never reaches the content path.
    fn lookup(&mut self, artifact: &Bytes) -> Lookup {
        if let Some(region) = self.by_region.get(&RegionIdentity::of(artifact)) {
            return Lookup::Hit(region.entry);
        }

        let content_hash = self.content_hasher.hash_one(&artifact[..]);
        let entries = &self.entries;
        let matched = self
            .by_content
            .find(content_hash, |slot| {
                slot.content_hash == content_hash
                    && entries
                        .peek(&slot.entry)
                        .expect("component cache content index names an evicted entry")
                        .artifact
                        == *artifact
            })
            .map(|slot| slot.entry);
        match matched {
            Some(entry) => {
                self.record_region(artifact, entry);
                Lookup::Hit(entry)
            }
            None => Lookup::Miss { content_hash },
        }
    }

    fn promote(&mut self, entry: EntryId) -> Arc<Component> {
        self.entries
            .get(&entry)
            .expect("component cache region index names an evicted entry")
            .component
            .clone()
    }

    fn record_region(&mut self, region: &Bytes, entry: EntryId) {
        self.by_region
            .retain(|_, recorded| !recorded.bytes.is_unique());
        let replaced = self.by_region.insert(
            RegionIdentity::of(region),
            Region {
                bytes: region.clone(),
                entry,
            },
        );
        assert!(
            replaced.is_none(),
            "component cache recorded a region it already resolves"
        );
    }

    fn evict_to_budget(&mut self) {
        while self.resident_bytes > self.budget_bytes {
            let Some((evicted, entry)) = self.entries.pop_lru() else {
                panic!("component cache accounting lost track of resident bytes");
            };
            let Ok(slot) = self
                .by_content
                .find_entry(entry.content_hash, |slot| slot.entry == evicted)
            else {
                panic!("component cache content index lost a resident entry");
            };
            slot.remove();
            self.by_region.retain(|_, region| region.entry != evicted);
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(entry.artifact.len())
                .expect("component cache byte accounting underflow");
        }
    }

    #[cfg(test)]
    fn recorded_regions(&self) -> usize {
        self.by_region.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::Cell;

    /// Counts how many content hashes the cache computes.
    #[derive(Clone, Default)]
    struct CountingHasher {
        hashes: Rc<Cell<usize>>,
        inner: DefaultHashBuilder,
    }

    impl BuildHasher for CountingHasher {
        type Hasher = <DefaultHashBuilder as BuildHasher>::Hasher;

        fn build_hasher(&self) -> Self::Hasher {
            self.hashes.set(self.hashes.get() + 1);
            self.inner.build_hasher()
        }
    }

    fn counting_cache<Component>(
        budget_bytes: usize,
    ) -> (ComponentCache<Component, CountingHasher>, Rc<Cell<usize>>) {
        let hasher = CountingHasher::default();
        let hashes = hasher.hashes.clone();
        (
            ComponentCache::with_content_hasher(budget_bytes, hasher),
            hashes,
        )
    }

    #[test]
    fn identity_lookup_uses_pointer_key_without_content_scan() {
        let (mut cache, hashes) = counting_cache(1024);
        let artifact = Bytes::from_static(b"component");
        let component = Arc::new(7_u32);
        let inserted = cache.insert_if_missing(artifact.clone(), component);
        assert_eq!(hashes.get(), 1);

        let hit = cache
            .get(&artifact)
            .expect("identity lookup should hit inserted artifact");
        assert!(Arc::ptr_eq(&inserted, &hit));
        assert_eq!(hashes.get(), 1);
    }

    #[test]
    fn identical_regions_pay_one_content_hash_each() {
        const LOOKUPS: usize = 64;
        let (mut cache, hashes) = counting_cache(1024);
        let first = Bytes::copy_from_slice(b"coreutils");
        let second = Bytes::copy_from_slice(b"coreutils");
        assert_ne!(first.as_ptr(), second.as_ptr());

        let inserted = cache.insert_if_missing(first.clone(), Arc::new(1_u32));
        for _ in 0..LOOKUPS {
            let hit = cache.get(&first).expect("first region should hit");
            assert!(Arc::ptr_eq(&inserted, &hit));
        }
        assert_eq!(hashes.get(), 1, "the inserting region hashes once");

        for _ in 0..LOOKUPS {
            let hit = cache.get(&second).expect("second region should hit");
            assert!(Arc::ptr_eq(&inserted, &hit));
            let hit = cache.get(&first).expect("first region should still hit");
            assert!(Arc::ptr_eq(&inserted, &hit));
        }
        assert_eq!(hashes.get(), 2, "each region hashes once");

        let kept = cache.insert_if_missing(second.clone(), Arc::new(2_u32));
        assert!(Arc::ptr_eq(&inserted, &kept));
        assert_eq!(
            hashes.get(),
            2,
            "inserting a matched region is a region hit"
        );
    }

    #[test]
    fn static_regions_with_identical_bytes_share_an_entry() {
        static FIRST: [u8; 4] = *b"wasm";
        static SECOND: [u8; 4] = *b"wasm";
        let (mut cache, hashes) = counting_cache(1024);
        let first = Bytes::from_static(&FIRST);
        let second = Bytes::from_static(&SECOND);
        let inserted = cache.insert_if_missing(first, Arc::new(1_u32));

        for _ in 0..8 {
            let hit = cache.get(&second).expect("second region should hit");
            assert!(Arc::ptr_eq(&inserted, &hit));
        }
        assert_eq!(hashes.get(), 2, "distinct statics are distinct regions");
    }

    #[test]
    fn dropped_heap_region_is_released_on_next_record() {
        let (mut cache, _hashes) = counting_cache(1024);
        let canonical = Bytes::copy_from_slice(b"artifact");
        cache.insert_if_missing(canonical.clone(), Arc::new(1_u32));
        drop(canonical);
        assert_eq!(cache.recorded_regions(), 1);

        let transient = Bytes::copy_from_slice(b"artifact");
        assert!(cache.get(&transient).is_some());
        assert_eq!(cache.recorded_regions(), 2);
        drop(transient);

        let live = Bytes::copy_from_slice(b"artifact");
        assert!(cache.get(&live).is_some());
        assert_eq!(
            cache.recorded_regions(),
            2,
            "the dropped region is released and the entry's own artifact stays"
        );
    }

    #[test]
    fn eviction_removes_identity_entry() {
        let mut cache = ComponentCache::new(4);
        let first_artifact = Bytes::from_static(b"aaaa");
        let second_artifact = Bytes::from_static(b"bbbb");
        let first_component = cache.insert_if_missing(first_artifact.clone(), Arc::new(1_u32));

        cache.insert_if_missing(second_artifact, Arc::new(2_u32));

        let replacement = cache.insert_if_missing(first_artifact, Arc::new(3_u32));
        assert!(!Arc::ptr_eq(&first_component, &replacement));
    }

    #[test]
    fn eviction_removes_every_region_of_the_entry() {
        let mut cache = ComponentCache::new(4);
        let first = Bytes::copy_from_slice(b"aaaa");
        let alias = Bytes::copy_from_slice(b"aaaa");
        cache.insert_if_missing(first.clone(), Arc::new(1_u32));
        assert!(cache.get(&alias).is_some());
        assert_eq!(cache.recorded_regions(), 2);

        cache.insert_if_missing(Bytes::copy_from_slice(b"bbbb"), Arc::new(2_u32));

        assert_eq!(cache.recorded_regions(), 1);
        assert!(cache.get(&first).is_none());
        assert!(cache.get(&alias).is_none());
    }

    #[test]
    fn region_hit_refreshes_recency() {
        let mut cache = ComponentCache::new(8);
        let first = Bytes::copy_from_slice(b"aaaa");
        let second = Bytes::copy_from_slice(b"bbbb");
        let first_component = cache.insert_if_missing(first.clone(), Arc::new(1_u32));
        cache.insert_if_missing(second.clone(), Arc::new(2_u32));

        assert!(cache.get(&first).is_some());
        cache.insert_if_missing(Bytes::copy_from_slice(b"cccc"), Arc::new(3_u32));

        let hit = cache
            .get(&first)
            .expect("the recently used entry should stay resident");
        assert!(Arc::ptr_eq(&first_component, &hit));
        assert!(cache.get(&second).is_none());
    }
}
