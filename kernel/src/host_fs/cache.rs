//! Bounded, time-limited caches for one 9p session.
//!
//! # Why a cache at all
//!
//! Every 9p operation starts at the session root: a `Twalk` per path,
//! a `Tgetattr` to learn what was walked, a `Tlopen` before any data
//! moves and a `Tclunk` afterwards. A CPython import walks the same
//! `sys.path` directories hundreds of times over, so the round trips
//! that answer "does this file exist" dominate the ones that carry
//! bytes. This module remembers those answers.
//!
//! # Coherence contract
//!
//! The host share is not exclusive: anything on the host may change a
//! file the kernel has already looked at, and 9p offers no invalidation
//! callback. Every table is therefore bounded in entries *and* in time.
//! An entry is authoritative for [`CACHE_TTL_NANOS`] and no longer, so
//! a host-side change is visible within that window without the client
//! ever having to be told about it. Mutations the kernel performs
//! itself do not wait for the TTL — they invalidate the paths they
//! touch as they happen.
//!
//! # Concurrency contract
//!
//! Each table is an independent `spin::Mutex`. Every method here is
//! synchronous and completes while holding one lock, so a lock is never
//! live across an await. Work that has to be async — clunking a fid the
//! cache gave up — is handed back to the caller as a value rather than
//! performed here.

extern crate alloc;

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicU64, Ordering};

use lru::LruCache;
use spin::Mutex as SpinMutex;

use crate::{HostDirEntry, HostMetadata};

/// How long an answer from the host stays authoritative.
///
/// Short enough that a host-side edit shows up in the guest while a
/// developer is still looking at the terminal, long enough to collapse
/// the burst of stats one interpreter import produces.
pub(super) const CACHE_TTL_NANOS: u64 = 2_000_000_000;

/// Paths the attribute table remembers, negative entries included.
const ATTRIBUTE_CACHE_ENTRIES: usize = 256;

/// Directory listings kept at once.
const DIRECTORY_CACHE_ENTRIES: usize = 16;

/// Directory entries the listing table may hold in total, across every
/// listing it caches. A single deep directory would otherwise decide the
/// table's footprint on its own.
const DIRECTORY_CACHE_ENTRY_BUDGET: usize = 4096;

/// Open read fids the pool parks for reuse.
///
/// The 9p protocol negotiates no fid budget, so this is the client's own
/// bound rather than one the server stated. It sits far below the open
/// descriptor budget a `virtfs` export keeps before it starts reclaiming
/// fids behind the client's back.
const READ_FID_POOL_ENTRIES: usize = 32;

/// How the host share's caches have served the kernel since boot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostFsCacheStats {
    /// Attribute lookups answered from a cached `Rgetattr`.
    pub attribute_hits: u64,
    /// Attribute lookups that had to walk and ask the host.
    pub attribute_misses: u64,
    /// Attribute lookups answered "no such path" without asking.
    pub negative_hits: u64,
    /// Directory listings answered from a cached `Treaddir` sweep.
    pub directory_hits: u64,
    /// Directory listings that had to be read from the host.
    pub directory_misses: u64,
    /// Reads that reused a fid the pool already had open.
    pub fid_hits: u64,
    /// Reads that had to walk and open a fid of their own.
    pub fid_misses: u64,
    /// Entries dropped because a table was full or an entry went stale.
    pub evictions: u64,
    /// Entries dropped because the kernel changed what they described.
    pub invalidations: u64,
}

/// The live counters behind [`HostFsCacheStats`].
#[derive(Debug, Default)]
struct Counters {
    attribute_hits: AtomicU64,
    attribute_misses: AtomicU64,
    negative_hits: AtomicU64,
    directory_hits: AtomicU64,
    directory_misses: AtomicU64,
    fid_hits: AtomicU64,
    fid_misses: AtomicU64,
    evictions: AtomicU64,
    invalidations: AtomicU64,
}

impl Counters {
    /// Counters are read for reporting, never to decide anything, so
    /// they are bumped without ordering constraints on anything else.
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn add(counter: &AtomicU64, amount: u64) {
        counter.fetch_add(amount, Ordering::Relaxed);
    }

    fn snapshot(&self) -> HostFsCacheStats {
        HostFsCacheStats {
            attribute_hits: self.attribute_hits.load(Ordering::Relaxed),
            attribute_misses: self.attribute_misses.load(Ordering::Relaxed),
            negative_hits: self.negative_hits.load(Ordering::Relaxed),
            directory_hits: self.directory_hits.load(Ordering::Relaxed),
            directory_misses: self.directory_misses.load(Ordering::Relaxed),
            fid_hits: self.fid_hits.load(Ordering::Relaxed),
            fid_misses: self.fid_misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            invalidations: self.invalidations.load(Ordering::Relaxed),
        }
    }
}

/// What the host said about one path, and when that stops counting.
struct AttributeEntry {
    /// `None` is a negative entry: the host reported no such path. It is
    /// what keeps a failed import candidate from being re-walked on
    /// every one of the dozens of `sys.path` probes that follow.
    metadata: Option<HostMetadata>,
    expires_nanos: u64,
}

/// One directory as the host listed it.
struct DirectoryEntry {
    entries: Vec<HostDirEntry>,
    expires_nanos: u64,
}

/// A fid the pool parks between reads of the same path.
struct ReadFidEntry {
    fid: u32,
    expires_nanos: u64,
}

/// What an attribute lookup found.
pub(super) enum CachedAttributes {
    /// The host's answer, still inside its TTL.
    Present(HostMetadata),
    /// The host said there is no such path, still inside its TTL.
    Absent,
    /// Nothing usable is cached; ask the host.
    Unknown,
}

/// What the read-fid pool had for a path.
///
/// A fid the pool gives up is still open on the server, so both of the
/// non-empty answers hand the caller a fid it owns — one to read from,
/// one to clunk.
pub(super) enum PooledReadFid {
    /// Open on the path and still inside its TTL: read from it.
    Reusable(u32),
    /// Past its TTL: clunk it, then open a fid of your own.
    Retired(u32),
    /// The pool holds nothing for this path.
    Absent,
}

/// The caches one 9p session keeps.
pub(super) struct HostFsCache {
    attributes: SpinMutex<LruCache<String, AttributeEntry>>,
    directories: SpinMutex<DirectoryTable>,
    read_fids: SpinMutex<LruCache<String, ReadFidEntry>>,
    counters: Counters,
}

/// Directory listings, bounded by listing count and by total entries.
struct DirectoryTable {
    listings: LruCache<String, DirectoryEntry>,
    resident_entries: usize,
}

impl HostFsCache {
    pub(super) fn new() -> Self {
        Self {
            attributes: SpinMutex::new(LruCache::new(capacity(ATTRIBUTE_CACHE_ENTRIES))),
            directories: SpinMutex::new(DirectoryTable {
                listings: LruCache::new(capacity(DIRECTORY_CACHE_ENTRIES)),
                resident_entries: 0,
            }),
            read_fids: SpinMutex::new(LruCache::new(capacity(READ_FID_POOL_ENTRIES))),
            counters: Counters::default(),
        }
    }

    pub(super) fn stats(&self) -> HostFsCacheStats {
        self.counters.snapshot()
    }

    /// Looks up what the host last said about `path`.
    pub(super) fn attributes(&self, path: &str, now_nanos: u64) -> CachedAttributes {
        let Some(key) = cache_key(path) else {
            Counters::bump(&self.counters.attribute_misses);
            return CachedAttributes::Unknown;
        };
        let mut attributes = self.attributes.lock();
        let Some(entry) = attributes.get(key.as_ref()) else {
            Counters::bump(&self.counters.attribute_misses);
            return CachedAttributes::Unknown;
        };
        if entry.expires_nanos <= now_nanos {
            attributes.pop(key.as_ref());
            Counters::bump(&self.counters.evictions);
            Counters::bump(&self.counters.attribute_misses);
            return CachedAttributes::Unknown;
        }
        match &entry.metadata {
            Some(metadata) => {
                let metadata = metadata.clone();
                Counters::bump(&self.counters.attribute_hits);
                CachedAttributes::Present(metadata)
            }
            None => {
                Counters::bump(&self.counters.negative_hits);
                CachedAttributes::Absent
            }
        }
    }

    /// Records what an `Rgetattr` said about `path`.
    pub(super) fn insert_attributes(&self, path: &str, metadata: &HostMetadata, now_nanos: u64) {
        self.put_attribute_entry(path, Some(metadata.clone()), now_nanos);
    }

    /// Records that the host reported no such path.
    pub(super) fn insert_missing(&self, path: &str, now_nanos: u64) {
        self.put_attribute_entry(path, None, now_nanos);
    }

    fn put_attribute_entry(&self, path: &str, metadata: Option<HostMetadata>, now_nanos: u64) {
        let Some(key) = cache_key(path) else {
            return;
        };
        let evicted = self.attributes.lock().push(
            key.into_owned(),
            AttributeEntry {
                metadata,
                expires_nanos: now_nanos.saturating_add(CACHE_TTL_NANOS),
            },
        );
        if evicted.is_some() {
            Counters::bump(&self.counters.evictions);
        }
    }

    /// Looks up the last listing the host gave for `path`.
    pub(super) fn directory(&self, path: &str, now_nanos: u64) -> Option<Vec<HostDirEntry>> {
        let Some(key) = cache_key(path) else {
            Counters::bump(&self.counters.directory_misses);
            return None;
        };
        let mut directories = self.directories.lock();
        let Some(entry) = directories.listings.get(key.as_ref()) else {
            Counters::bump(&self.counters.directory_misses);
            return None;
        };
        if entry.expires_nanos <= now_nanos {
            directories.remove(key.as_ref());
            Counters::bump(&self.counters.evictions);
            Counters::bump(&self.counters.directory_misses);
            return None;
        }
        let entries = entry.entries.clone();
        Counters::bump(&self.counters.directory_hits);
        Some(entries)
    }

    /// Records the listing a `Treaddir` sweep produced for `path`.
    pub(super) fn insert_directory(&self, path: &str, entries: &[HostDirEntry], now_nanos: u64) {
        let Some(key) = cache_key(path) else {
            return;
        };
        let mut directories = self.directories.lock();
        let evicted = directories.insert(
            key.into_owned(),
            DirectoryEntry {
                entries: entries.to_vec(),
                expires_nanos: now_nanos.saturating_add(CACHE_TTL_NANOS),
            },
        );
        Counters::add(&self.counters.evictions, evicted);
    }

    /// Takes the pooled read fid for `path`, if the pool has one.
    pub(super) fn take_read_fid(&self, path: &str, now_nanos: u64) -> PooledReadFid {
        let Some(key) = cache_key(path) else {
            Counters::bump(&self.counters.fid_misses);
            return PooledReadFid::Absent;
        };
        let Some(entry) = self.read_fids.lock().pop(key.as_ref()) else {
            Counters::bump(&self.counters.fid_misses);
            return PooledReadFid::Absent;
        };
        if entry.expires_nanos <= now_nanos {
            Counters::bump(&self.counters.evictions);
            Counters::bump(&self.counters.fid_misses);
            return PooledReadFid::Retired(entry.fid);
        }
        Counters::bump(&self.counters.fid_hits);
        PooledReadFid::Reusable(entry.fid)
    }

    /// Parks an open read fid for the next read of `path`.
    ///
    /// Returns the fid the pool gave up to make room, which the caller
    /// owes the server a `Tclunk` for. An uncacheable path parks
    /// nothing and hands its own fid straight back.
    #[must_use]
    pub(super) fn park_read_fid(&self, path: &str, fid: u32, now_nanos: u64) -> Option<u32> {
        let Some(key) = cache_key(path) else {
            return Some(fid);
        };
        let evicted = self.read_fids.lock().push(
            key.into_owned(),
            ReadFidEntry {
                fid,
                expires_nanos: now_nanos.saturating_add(CACHE_TTL_NANOS),
            },
        );
        evicted.map(|(_, entry)| {
            Counters::bump(&self.counters.evictions);
            entry.fid
        })
    }

    /// Drops everything the cache knows about `path` after the kernel
    /// changed the file's contents or attributes.
    ///
    /// The parent's listing survives: writing a file changes neither the
    /// names in its directory nor which of them are directories, so a
    /// write has no bearing on a cached `Treaddir`.
    #[must_use]
    pub(super) fn invalidate_contents(&self, path: &str) -> Option<u32> {
        let key = cache_key(path)?;
        Counters::bump(&self.counters.invalidations);
        self.attributes.lock().pop(key.as_ref());
        self.directories.lock().remove(key.as_ref());
        self.read_fids
            .lock()
            .pop(key.as_ref())
            .map(|entry| entry.fid)
    }

    /// Drops everything the cache knows about `path` after the kernel
    /// created, removed, renamed or linked it.
    ///
    /// The parent directory is invalidated too: its listing gained or
    /// lost a name, and the host bumped its modification time doing so.
    #[must_use]
    pub(super) fn invalidate_entry(&self, path: &str) -> Option<u32> {
        let orphaned = self.invalidate_contents(path);
        // The parent is derived from the normalised key, not from the
        // caller's spelling: the guest side of the share hands paths
        // over without a leading slash, and splitting one of those
        // would find no parent and leave the directory's listing
        // claiming a name that is no longer there.
        let Some(key) = cache_key(path) else {
            return orphaned;
        };
        if let Some(parent) = parent_key(key.as_ref()) {
            let dropped = self.invalidate_contents(parent);
            debug_assert!(
                dropped.is_none(),
                "a directory is never parked in the read-fid pool"
            );
        }
        orphaned
    }
}

impl DirectoryTable {
    /// Inserts a listing, evicting until both bounds hold again.
    /// Returns how many listings were dropped to make room.
    fn insert(&mut self, key: String, entry: DirectoryEntry) -> u64 {
        let arriving = entry.entries.len();
        let mut evicted = 0;
        if let Some((_, replaced)) = self.listings.push(key, entry) {
            self.forget(replaced.entries.len());
            evicted += 1;
        }
        self.resident_entries = self
            .resident_entries
            .checked_add(arriving)
            .expect("host-fs directory cache entry accounting overflowed");
        while self.resident_entries > DIRECTORY_CACHE_ENTRY_BUDGET {
            let Some((_, dropped)) = self.listings.pop_lru() else {
                panic!("host-fs directory cache accounting lost track of its entries");
            };
            self.forget(dropped.entries.len());
            evicted += 1;
        }
        evicted
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.listings.pop(key) {
            self.forget(entry.entries.len());
        }
    }

    fn forget(&mut self, entries: usize) {
        self.resident_entries = self
            .resident_entries
            .checked_sub(entries)
            .expect("host-fs directory cache entry accounting underflowed");
    }
}

fn capacity(entries: usize) -> NonZeroUsize {
    NonZeroUsize::new(entries).expect("a host-fs cache table holds at least one entry")
}

/// The key `path` is cached under, or `None` when it must not be cached.
///
/// Two spellings of one path have to collide, or a write through one of
/// them would leave a stale answer under the other. Segments are
/// therefore normalised — empty components and trailing slashes go away
/// — without allocating for the already-normal spelling that the
/// component layer produces. A path carrying `.` or `..` names no fixed file
/// until the server has resolved it, so it is not cached at all rather
/// than cached under a key that may not mean what it says.
fn cache_key(path: &str) -> Option<Cow<'_, str>> {
    let mut segments = 0;
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." {
            return None;
        }
        segments += 1;
    }
    if segments == 0 {
        return Some(Cow::Borrowed("/"));
    }
    if is_normalised(path) {
        return Some(Cow::Borrowed(path));
    }
    let mut key = String::with_capacity(path.len());
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        key.push('/');
        key.push_str(segment);
    }
    Some(Cow::Owned(key))
}

/// Whether `path` is already spelled the way [`cache_key`] would spell
/// it: a leading slash, one slash between segments, none at the end.
fn is_normalised(path: &str) -> bool {
    path.starts_with('/') && !path.ends_with('/') && !path.contains("//")
}

/// The directory a normalised key sits in, or `None` for the share
/// root, which has no parent inside the share.
fn parent_key(key: &str) -> Option<&str> {
    debug_assert!(
        key.starts_with('/'),
        "a parent is only well defined for a normalised cache key"
    );
    let (parent, name) = key.rsplit_once('/')?;
    if name.is_empty() {
        return None;
    }
    Some(if parent.is_empty() { "/" } else { parent })
}

impl HostFsCacheStats {
    /// Total lookups the caches answered without a round trip.
    pub fn hits(&self) -> u64 {
        self.attribute_hits
            .saturating_add(self.negative_hits)
            .saturating_add(self.directory_hits)
            .saturating_add(self.fid_hits)
    }

    /// Total lookups that had to ask the host.
    pub fn misses(&self) -> u64 {
        self.attribute_misses
            .saturating_add(self.directory_misses)
            .saturating_add(self.fid_misses)
    }
}

impl core::fmt::Display for HostFsCacheStats {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "hits={} misses={} evictions={} invalidations={}",
            self.hits(),
            self.misses(),
            self.evictions,
            self.invalidations
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn metadata(size: u64) -> HostMetadata {
        HostMetadata {
            identity: crate::ObjectIdentity::new(crate::AuthorityDomain::HOST_SHARE_9P, 1),
            qid_path: 1,
            qid_type: 0,
            mode: 0o100_644,
            size,
            link_count: 1,
            access_nanos: 0,
            modified_nanos: 0,
            status_nanos: 0,
        }
    }

    #[test]
    fn spellings_of_one_path_share_a_cache_key() {
        assert_eq!(cache_key("/alpha/beta").as_deref(), Some("/alpha/beta"));
        assert_eq!(cache_key("/alpha//beta/").as_deref(), Some("/alpha/beta"));
        assert_eq!(cache_key("alpha/beta").as_deref(), Some("/alpha/beta"));
        assert_eq!(cache_key("/").as_deref(), Some("/"));
        assert_eq!(cache_key("").as_deref(), Some("/"));
    }

    #[test]
    fn an_unresolved_path_is_not_cached() {
        assert!(cache_key("/alpha/../beta").is_none());
        assert!(cache_key("/alpha/./beta").is_none());
    }

    #[test]
    fn the_already_normal_spelling_is_not_reallocated() {
        assert!(matches!(cache_key("/alpha/beta"), Some(Cow::Borrowed(_))));
        assert!(matches!(cache_key("/alpha//beta"), Some(Cow::Owned(_))));
    }

    #[test]
    fn parent_keys_stop_at_the_share_root() {
        assert_eq!(parent_key("/alpha/beta"), Some("/alpha"));
        assert_eq!(parent_key("/alpha"), Some("/"));
        assert_eq!(parent_key("/"), None);
    }

    /// The guest side of the share strips the mount prefix and hands
    /// paths over without a leading slash, so an entry created that way
    /// has to reach the same directory listing a rooted read cached.
    #[test]
    fn an_unrooted_spelling_still_invalidates_the_parent_listing() {
        let cache = HostFsCache::new();
        cache.insert_directory(
            "/",
            &[HostDirEntry {
                name: "greeting.txt".to_string(),
                is_directory: false,
            }],
            0,
        );

        assert!(cache.invalidate_entry("newdir").is_none());

        assert!(
            cache.directory("/", 1).is_none(),
            "the share root gained a name, so its cached listing is stale"
        );
    }

    #[test]
    fn an_attribute_entry_expires_with_its_ttl() {
        let cache = HostFsCache::new();
        cache.insert_attributes("/alpha", &metadata(7), 0);

        assert!(matches!(
            cache.attributes("/alpha", CACHE_TTL_NANOS - 1),
            CachedAttributes::Present(found) if found.size == 7
        ));
        assert!(matches!(
            cache.attributes("/alpha", CACHE_TTL_NANOS),
            CachedAttributes::Unknown
        ));
    }

    #[test]
    fn a_negative_entry_answers_without_asking_the_host() {
        let cache = HostFsCache::new();
        cache.insert_missing("/alpha", 0);

        assert!(matches!(
            cache.attributes("/alpha", 1),
            CachedAttributes::Absent
        ));
        assert_eq!(cache.stats().negative_hits, 1);
        assert_eq!(cache.stats().attribute_misses, 0);
    }

    #[test]
    fn creating_an_entry_invalidates_the_parent_listing() {
        let cache = HostFsCache::new();
        cache.insert_directory(
            "/alpha",
            &[HostDirEntry {
                name: "beta".to_string(),
                is_directory: false,
            }],
            0,
        );
        cache.insert_attributes("/alpha/beta", &metadata(1), 0);

        assert!(cache.invalidate_entry("/alpha/beta").is_none());

        assert!(cache.directory("/alpha", 1).is_none());
        assert!(matches!(
            cache.attributes("/alpha/beta", 1),
            CachedAttributes::Unknown
        ));
    }

    #[test]
    fn writing_a_file_leaves_its_directory_listing_alone() {
        let cache = HostFsCache::new();
        cache.insert_directory(
            "/alpha",
            &[HostDirEntry {
                name: "beta".to_string(),
                is_directory: false,
            }],
            0,
        );
        cache.insert_attributes("/alpha/beta", &metadata(1), 0);

        assert!(cache.invalidate_contents("/alpha/beta").is_none());

        assert_eq!(
            cache.directory("/alpha", 1).map(|entries| entries.len()),
            Some(1)
        );
        assert!(matches!(
            cache.attributes("/alpha/beta", 1),
            CachedAttributes::Unknown
        ));
    }

    #[test]
    fn the_read_fid_pool_hands_a_parked_fid_back_once() {
        let cache = HostFsCache::new();

        assert!(cache.park_read_fid("/alpha", 9, 0).is_none());

        assert!(matches!(
            cache.take_read_fid("/alpha", 1),
            PooledReadFid::Reusable(9)
        ));
        assert!(matches!(
            cache.take_read_fid("/alpha", 1),
            PooledReadFid::Absent
        ));
    }

    #[test]
    fn a_parked_fid_past_its_ttl_is_retired_rather_than_reused() {
        let cache = HostFsCache::new();
        assert!(cache.park_read_fid("/alpha", 9, 0).is_none());

        assert!(matches!(
            cache.take_read_fid("/alpha", CACHE_TTL_NANOS),
            PooledReadFid::Retired(9)
        ));
    }

    #[test]
    fn a_full_read_fid_pool_gives_up_its_least_recent_fid() {
        let cache = HostFsCache::new();
        let mut path = String::new();
        for index in 0..READ_FID_POOL_ENTRIES {
            path.clear();
            path.push_str("/file");
            path.push_str(&index.to_string());
            assert!(cache.park_read_fid(&path, index as u32, 0).is_none());
        }

        let orphaned = cache.park_read_fid("/one-too-many", 999, 0);

        assert_eq!(
            orphaned,
            Some(0),
            "the least recently parked fid is given up"
        );
    }

    #[test]
    fn an_uncacheable_path_parks_nothing_and_hands_its_fid_back() {
        let cache = HostFsCache::new();

        assert_eq!(cache.park_read_fid("/alpha/../beta", 4, 0), Some(4));
    }

    #[test]
    fn the_listing_table_stays_within_its_entry_budget() {
        let cache = HostFsCache::new();
        let listing: Vec<HostDirEntry> = (0..DIRECTORY_CACHE_ENTRY_BUDGET)
            .map(|index| HostDirEntry {
                name: index.to_string(),
                is_directory: false,
            })
            .collect();

        cache.insert_directory("/first", &listing, 0);
        cache.insert_directory("/second", &listing, 0);

        assert!(
            cache.directory("/first", 1).is_none(),
            "the budget cannot hold two full listings, so the older one goes"
        );
        assert!(cache.directory("/second", 1).is_some());
    }
}
