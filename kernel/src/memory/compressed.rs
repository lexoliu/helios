//! The compressed page pool: kernel memory that holds pages compressed,
//! so a page a subsystem would otherwise drop or write to a disk can stay
//! in memory at a fraction of its size.
//!
//! A client hands the pool a page and gets back a [`CompressedPage`]: an
//! owned value holding the compressed bytes, charged to the pool for as
//! long as it lives. The client keeps it wherever its own bookkeeping
//! keeps the page — the block cache keeps it on the cache entry — and
//! decompresses it when the page is wanted again. Dropping the value is
//! the eviction; the client decides what has to happen to the page
//! first, because only the client knows whether the page differs from
//! its backing store.
//!
//! Pages are compressed with zstd (`zrip`, the pure-Rust encoder and
//! decoder) at [`DEFAULT_COMPRESSION_LEVEL`], the operator's knob.
//!
//! # Concurrency contract
//!
//! - **Compression and decompression run on the caller's processor** and
//!   take no lock shared with another processor. Decompression reuses
//!   the executing processor's decoder workspace, built the first time
//!   that processor decompresses, behind a spin lock that only tasks on
//!   that processor take; nothing an interrupt handler runs reaches it.
//!   Compression goes through zrip's one-shot encoder, which builds its
//!   match state per call: zrip keeps a reusable encoder context only in
//!   its `std` build.
//! - **Accounting is lock-free.** Storing a page reserves its bytes
//!   against the limit with one compare-and-swap loop on the pool's
//!   stored-bytes word; dropping a page gives them back with one
//!   subtraction. The limit and the level are atomics an operator may
//!   change at any time; a change applies to the next page stored.
//! - **Counters are per processor.** Each processor's row of event
//!   counters is written only by tasks running on that processor and
//!   lives on its own cache line; [`CompressedPool::stats`] sums the
//!   rows. The shared words — the stored bytes and pages every store and
//!   drop moves, and the settings an operator writes — sit on lines of
//!   their own.
//! - **A compressed page is an owned value.** The pool keeps no index of
//!   the pages it holds, so there is no pool-wide lock and no list to
//!   walk. The order pages leave in is the client's eviction policy.
//!
//! # Prior art
//!
//! The pool is the kernel's zswap: Linux compresses pages on their way
//! to swap into a pool bounded by a share of memory
//! (`zswap_max_pool_percent`, 20 by default, `mm/zswap.c`) and refuses a
//! page that does not compress well enough to be worth holding. The
//! threshold is zram's: a page whose compressed form is
//! [`INCOMPRESSIBLE_BYTES`] or longer is stored as incompressible
//! (`zram_write_page`, `drivers/block/zram/zram_drv.c`), because
//! `huge_class_size` — the smallest compressed size zsmalloc cannot pack
//! more than one of into its size classes — is where compression stops
//! saving memory.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use crossbeam_utils::CachePadded;
use helios_hal::cpu::current_processor;
use helios_hal::pmm::PhysFrame;
use spin::Mutex;
use zrip::{CompressError, DecompressContext};

/// Bytes in the pages the pool compresses.
pub const COMPRESSED_PAGE_BYTES: usize = PhysFrame::SIZE;

/// The zstd level pages are compressed at until an operator chooses
/// another: zstd's fastest level that still entropy-codes literals, the
/// level zswap and zram configure for zstd.
pub const DEFAULT_COMPRESSION_LEVEL: i32 = 1;

/// Compressed size at and above which a page is incompressible and skips
/// the pool.
///
/// zram's `huge_class_size` for a 4 KiB page: zsmalloc's size classes
/// start at 32 bytes and step by 16 (`ZS_MIN_ALLOC_SIZE`,
/// `ZS_SIZE_CLASS_DELTA`), and the first class whose zspages of up to
/// eight pages (`CONFIG_ZSMALLOC_CHAIN_SIZE`) hold one object per page is
/// 3632 bytes; `zs_huge_class_size` subtracts the 7 bytes of a handle
/// past one byte, giving 3625 — 88.5 % of the page. zswap refuses only a
/// page that does not shrink at all; zram's bound is the one that tracks
/// what holding the page compressed actually saves.
pub const INCOMPRESSIBLE_BYTES: usize = 3625;

const _: () = assert!(
    COMPRESSED_PAGE_BYTES == 4096,
    "INCOMPRESSIBLE_BYTES is zsmalloc's huge class for a 4 KiB page"
);

/// Share of the machine's usable memory the pool may hold by default,
/// in percent: zswap's `max_pool_percent` default.
pub const DEFAULT_POOL_PERCENT: usize = 20;

/// Why the pool did not take a page.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompressRefusal {
    #[error("the page compresses to {INCOMPRESSIBLE_BYTES} bytes or more")]
    Incompressible,
    #[error("holding {needed} more bytes would pass the pool's {limit_bytes}-byte limit")]
    Full { needed: usize, limit_bytes: usize },
}

/// A setting the pool cannot take.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CompressedPoolError {
    #[error("zstd has no compression level {0}")]
    InvalidLevel(i32),
}

/// What the pool holds and has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressedPoolStats {
    /// Compressed bytes held now.
    pub stored_bytes: usize,
    /// Pages held now.
    pub stored_pages: usize,
    /// Most compressed bytes the pool may hold.
    pub limit_bytes: usize,
    /// Pages ever stored.
    pub stores: u64,
    /// Pages ever decompressed.
    pub loads: u64,
    /// Pages refused as incompressible.
    pub incompressible: u64,
    /// Pages refused because the pool was at its limit.
    pub limit_hits: u64,
}

/// What the pool holds, moved by every store and every drop.
struct Holdings {
    stored_bytes: AtomicUsize,
    stored_pages: AtomicUsize,
}

/// The operator's settings: written rarely, read by every store.
struct Settings {
    limit_bytes: AtomicUsize,
    level: AtomicI32,
}

/// One processor's zstd state.
#[derive(Default)]
struct Workspace {
    decoder: Option<DecompressContext>,
}

/// What one processor's tasks have asked of the pool.
#[derive(Default)]
struct Counters {
    stores: AtomicU64,
    loads: AtomicU64,
    incompressible: AtomicU64,
    limit_hits: AtomicU64,
}

/// One processor's share of the pool: its workspace and its counters,
/// both touched only by tasks running on it.
#[derive(Default)]
struct ProcessorRow {
    workspace: Mutex<Workspace>,
    counters: Counters,
}

struct PoolShared {
    holdings: CachePadded<Holdings>,
    settings: CachePadded<Settings>,
    /// Indexed by processor.
    processors: Box<[CachePadded<ProcessorRow>]>,
}

impl PoolShared {
    /// The executing processor's row.
    fn row(&self) -> &ProcessorRow {
        let processor = current_processor().id() as usize;
        self.processors.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no row among the compressed pool's {}",
                self.processors.len()
            )
        })
    }
}

/// The kernel's compressed page pool. Clones share one pool.
#[derive(Clone)]
pub struct CompressedPool {
    shared: Arc<PoolShared>,
}

/// One page held compressed, charged to its pool until it is dropped.
pub struct CompressedPage {
    bytes: Box<[u8]>,
    pool: Arc<PoolShared>,
}

/// Whether zstd has the compression level `level`. The level table is
/// the encoder's; it is asked rather than copied.
fn level_exists(level: i32) -> bool {
    zrip::encode::strategy::level_params(level).is_some()
}

impl CompressedPool {
    /// A pool that may hold `limit_bytes` of compressed pages, for a
    /// machine of `processors` processors.
    pub fn new(limit_bytes: usize, processors: usize) -> Self {
        assert!(
            processors != 0,
            "the compressed pool needs at least one processor"
        );
        Self {
            shared: Arc::new(PoolShared {
                holdings: CachePadded::new(Holdings {
                    stored_bytes: AtomicUsize::new(0),
                    stored_pages: AtomicUsize::new(0),
                }),
                settings: CachePadded::new(Settings {
                    limit_bytes: AtomicUsize::new(limit_bytes),
                    level: AtomicI32::new(DEFAULT_COMPRESSION_LEVEL),
                }),
                processors: (0..processors)
                    .map(|_| CachePadded::new(ProcessorRow::default()))
                    .collect(),
            }),
        }
    }

    /// The pool a machine of `usable_bytes` gets by default:
    /// [`DEFAULT_POOL_PERCENT`] of it.
    pub fn for_machine(usable_bytes: usize, processors: usize) -> Self {
        Self::new(usable_bytes / 100 * DEFAULT_POOL_PERCENT, processors)
    }

    /// Sets the most compressed bytes the pool may hold. Pages already
    /// held stay; a lower limit refuses new ones until enough leave.
    pub fn set_limit_bytes(&self, limit_bytes: usize) {
        self.shared
            .settings
            .limit_bytes
            .store(limit_bytes, Ordering::Relaxed);
    }

    /// Sets the zstd level the next pages are compressed at.
    pub fn set_level(&self, level: i32) -> Result<(), CompressedPoolError> {
        if !level_exists(level) {
            return Err(CompressedPoolError::InvalidLevel(level));
        }
        self.shared.settings.level.store(level, Ordering::Relaxed);
        Ok(())
    }

    /// The zstd level pages are compressed at.
    pub fn level(&self) -> i32 {
        self.shared.settings.level.load(Ordering::Relaxed)
    }

    /// Whether `bytes` more compressed bytes fit under the limit now.
    pub fn has_room_for(&self, bytes: usize) -> bool {
        let shared = &self.shared;
        shared
            .holdings
            .stored_bytes
            .load(Ordering::Relaxed)
            .saturating_add(bytes)
            <= shared.settings.limit_bytes.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> CompressedPoolStats {
        let shared = &self.shared;
        let mut stats = CompressedPoolStats {
            stored_bytes: shared.holdings.stored_bytes.load(Ordering::Relaxed),
            stored_pages: shared.holdings.stored_pages.load(Ordering::Relaxed),
            limit_bytes: shared.settings.limit_bytes.load(Ordering::Relaxed),
            ..CompressedPoolStats::default()
        };
        for row in shared.processors.iter() {
            let counters = &row.counters;
            stats.stores += counters.stores.load(Ordering::Relaxed);
            stats.loads += counters.loads.load(Ordering::Relaxed);
            stats.incompressible += counters.incompressible.load(Ordering::Relaxed);
            stats.limit_hits += counters.limit_hits.load(Ordering::Relaxed);
        }
        stats
    }

    /// Compresses `page` into the pool.
    pub fn compress(
        &self,
        page: &[u8; COMPRESSED_PAGE_BYTES],
    ) -> Result<CompressedPage, CompressRefusal> {
        let shared = &self.shared;
        let row = shared.row();
        // A frame that does not fit below the threshold is incompressible;
        // the encoder stops as soon as it runs out of room.
        let mut frame = [0u8; INCOMPRESSIBLE_BYTES - 1];
        let level = shared.settings.level.load(Ordering::Relaxed);
        let len = match zrip::compress_into(page, &mut frame, level) {
            Ok(len) => len,
            Err(CompressError::OutputTooSmall) => {
                row.counters.incompressible.fetch_add(1, Ordering::Relaxed);
                return Err(CompressRefusal::Incompressible);
            }
            Err(error) => panic!("zstd refused a page at a level the pool accepted: {error:?}"),
        };
        let holdings = &shared.holdings;
        let mut stored = holdings.stored_bytes.load(Ordering::Relaxed);
        loop {
            let limit_bytes = shared.settings.limit_bytes.load(Ordering::Relaxed);
            if stored.saturating_add(len) > limit_bytes {
                row.counters.limit_hits.fetch_add(1, Ordering::Relaxed);
                return Err(CompressRefusal::Full {
                    needed: len,
                    limit_bytes,
                });
            }
            match holdings.stored_bytes.compare_exchange_weak(
                stored,
                stored + len,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => stored = observed,
            }
        }
        holdings.stored_pages.fetch_add(1, Ordering::Relaxed);
        row.counters.stores.fetch_add(1, Ordering::Relaxed);
        Ok(CompressedPage {
            bytes: Box::from(&frame[..len]),
            pool: Arc::clone(shared),
        })
    }
}

impl CompressedPage {
    /// Compressed bytes the page holds.
    pub fn compressed_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Writes every byte of the page's contents to `page`.
    pub fn decompress_into(&self, page: &mut [u8; COMPRESSED_PAGE_BYTES]) {
        let row = self.pool.row();
        let mut workspace = row.workspace.lock();
        let decoded = workspace
            .decoder
            .get_or_insert_with(DecompressContext::new)
            .decompress_with_limit(&self.bytes, COMPRESSED_PAGE_BYTES)
            .unwrap_or_else(|error| {
                panic!("a page the pool compressed does not decompress: {error:?}")
            });
        assert_eq!(
            decoded.len(),
            COMPRESSED_PAGE_BYTES,
            "a compressed page decompressed to the wrong length"
        );
        page.copy_from_slice(&decoded);
        row.counters.loads.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for CompressedPage {
    fn drop(&mut self) {
        let holdings = &self.pool.holdings;
        holdings
            .stored_bytes
            .fetch_sub(self.bytes.len(), Ordering::Relaxed);
        holdings.stored_pages.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_page() -> [u8; COMPRESSED_PAGE_BYTES] {
        let line = "block 0042: 春眠不覚暁 / 春はあけぼの / 봄날은 간다\n".as_bytes();
        core::array::from_fn(|index| line[index % line.len()])
    }

    fn noise_page(seed: u64) -> [u8; COMPRESSED_PAGE_BYTES] {
        let mut state = seed | 1;
        core::array::from_fn(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
    }

    #[test]
    fn a_page_round_trips_and_is_charged_until_dropped() {
        let pool = CompressedPool::new(1 << 20, 1);
        let page = text_page();
        let compressed = pool.compress(&page).expect("text compresses");
        assert!(compressed.compressed_bytes() < INCOMPRESSIBLE_BYTES);
        let stats = pool.stats();
        assert_eq!(stats.stored_pages, 1);
        assert_eq!(stats.stored_bytes, compressed.compressed_bytes());

        let mut out = [0_u8; COMPRESSED_PAGE_BYTES];
        compressed.decompress_into(&mut out);
        assert_eq!(out, page);
        assert_eq!(pool.stats().loads, 1);

        drop(compressed);
        let stats = pool.stats();
        assert_eq!((stats.stored_pages, stats.stored_bytes), (0, 0));
    }

    #[test]
    fn an_incompressible_page_is_refused() {
        let pool = CompressedPool::new(1 << 20, 1);
        assert_eq!(
            pool.compress(&noise_page(7)).err(),
            Some(CompressRefusal::Incompressible)
        );
        let stats = pool.stats();
        assert_eq!((stats.incompressible, stats.stored_pages), (1, 0));
    }

    #[test]
    fn the_limit_refuses_pages_until_room_is_given_back() {
        let pool = CompressedPool::new(1 << 20, 1);
        let first = pool.compress(&text_page()).expect("room");
        pool.set_limit_bytes(first.compressed_bytes());
        assert!(!pool.has_room_for(1));
        assert!(matches!(
            pool.compress(&text_page()),
            Err(CompressRefusal::Full { .. })
        ));
        assert_eq!(pool.stats().limit_hits, 1);
        drop(first);
        assert!(pool.has_room_for(1));
        pool.compress(&text_page()).expect("room again");
    }

    #[test]
    fn the_level_is_an_operator_setting_zstd_validates() {
        let pool = CompressedPool::new(1 << 20, 1);
        assert_eq!(pool.level(), DEFAULT_COMPRESSION_LEVEL);
        let page = text_page();
        let at_default = pool.compress(&page).expect("compresses");
        pool.set_level(3).expect("zstd level 3");
        assert_eq!(pool.level(), 3);
        assert_eq!(
            pool.set_level(99),
            Err(CompressedPoolError::InvalidLevel(99))
        );
        assert_eq!(pool.level(), 3);
        // A page stored at the old level still decodes after the change.
        let at_three = pool.compress(&page).expect("compresses");
        let mut out = [0_u8; COMPRESSED_PAGE_BYTES];
        at_three.decompress_into(&mut out);
        assert_eq!(out, page);
        at_default.decompress_into(&mut out);
        assert_eq!(out, page);
    }

    #[test]
    fn each_processor_counts_into_its_own_row() {
        let pool = CompressedPool::new(1 << 20, 2);
        let page = text_page();
        crate::test_processor_identity::set(helios_hal::cpu::ProcessorId::new(0));
        let first = pool.compress(&page).expect("compresses");
        crate::test_processor_identity::set(helios_hal::cpu::ProcessorId::new(1));
        let second = pool.compress(&page).expect("compresses");
        let mut out = [0_u8; COMPRESSED_PAGE_BYTES];
        first.decompress_into(&mut out);
        crate::test_processor_identity::set(helios_hal::cpu::ProcessorId::new(0));
        second.decompress_into(&mut out);

        let stats = pool.stats();
        assert_eq!((stats.stores, stats.loads, stats.stored_pages), (2, 2, 2));
        assert_eq!(
            pool.shared.processors[1]
                .counters
                .stores
                .load(Ordering::Relaxed),
            1
        );
    }
}
