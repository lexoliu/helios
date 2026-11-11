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
//!   the executing processor's decoder workspace, behind a spin lock that
//!   only tasks on that processor take; nothing an interrupt handler
//!   runs reaches it.
//! - **Accounting is lock-free.** Storing a page reserves its bytes
//!   against the limit with one compare-and-swap loop on the pool's
//!   stored-bytes word; dropping a page gives them back with one
//!   subtraction. The limit and the level are atomics an operator may
//!   change at any time; a change applies to the next page stored.
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
use core::mem::MaybeUninit;
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

struct PoolShared {
    stored_bytes: CachePadded<AtomicUsize>,
    stored_pages: AtomicUsize,
    limit_bytes: AtomicUsize,
    level: AtomicI32,
    stores: AtomicU64,
    loads: AtomicU64,
    incompressible: AtomicU64,
    limit_hits: AtomicU64,
    /// Indexed by processor; row `p` is taken only by tasks running on
    /// processor `p`, and built the first time one decompresses there.
    decoders: Box<[CachePadded<Mutex<Option<DecompressContext>>>]>,
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
                stored_bytes: CachePadded::new(AtomicUsize::new(0)),
                stored_pages: AtomicUsize::new(0),
                limit_bytes: AtomicUsize::new(limit_bytes),
                level: AtomicI32::new(DEFAULT_COMPRESSION_LEVEL),
                stores: AtomicU64::new(0),
                loads: AtomicU64::new(0),
                incompressible: AtomicU64::new(0),
                limit_hits: AtomicU64::new(0),
                decoders: (0..processors)
                    .map(|_| CachePadded::new(Mutex::new(None)))
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
            .limit_bytes
            .store(limit_bytes, Ordering::Relaxed);
    }

    /// Sets the zstd level the next pages are compressed at.
    pub fn set_level(&self, level: i32) -> Result<(), CompressedPoolError> {
        // zstd's level table is the encoder's; ask it rather than copy it.
        let mut frame = [0u8; 32];
        match zrip::compress_into(&[], &mut frame, level) {
            Ok(_) => {
                self.shared.level.store(level, Ordering::Relaxed);
                Ok(())
            }
            Err(CompressError::InvalidLevel(level)) => {
                Err(CompressedPoolError::InvalidLevel(level))
            }
            Err(error) => panic!("an empty zstd frame did not fit 32 bytes: {error:?}"),
        }
    }

    /// The zstd level pages are compressed at.
    pub fn level(&self) -> i32 {
        self.shared.level.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> CompressedPoolStats {
        let shared = &self.shared;
        CompressedPoolStats {
            stored_bytes: shared.stored_bytes.load(Ordering::Relaxed),
            stored_pages: shared.stored_pages.load(Ordering::Relaxed),
            limit_bytes: shared.limit_bytes.load(Ordering::Relaxed),
            stores: shared.stores.load(Ordering::Relaxed),
            loads: shared.loads.load(Ordering::Relaxed),
            incompressible: shared.incompressible.load(Ordering::Relaxed),
            limit_hits: shared.limit_hits.load(Ordering::Relaxed),
        }
    }

    /// Compresses `page` into the pool.
    pub fn compress(
        &self,
        page: &[u8; COMPRESSED_PAGE_BYTES],
    ) -> Result<CompressedPage, CompressRefusal> {
        let shared = &self.shared;
        // A frame that does not fit below the threshold is incompressible;
        // the encoder stops as soon as it runs out of room.
        let mut frame = [0u8; INCOMPRESSIBLE_BYTES - 1];
        let len = match zrip::compress_into(page, &mut frame, shared.level.load(Ordering::Relaxed))
        {
            Ok(len) => len,
            Err(CompressError::OutputTooSmall) => {
                shared.incompressible.fetch_add(1, Ordering::Relaxed);
                return Err(CompressRefusal::Incompressible);
            }
            Err(error) => panic!("zstd refused a page at a level the pool accepted: {error:?}"),
        };
        let mut stored = shared.stored_bytes.load(Ordering::Relaxed);
        loop {
            let limit_bytes = shared.limit_bytes.load(Ordering::Relaxed);
            if stored.saturating_add(len) > limit_bytes {
                shared.limit_hits.fetch_add(1, Ordering::Relaxed);
                return Err(CompressRefusal::Full {
                    needed: len,
                    limit_bytes,
                });
            }
            match shared.stored_bytes.compare_exchange_weak(
                stored,
                stored + len,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => stored = observed,
            }
        }
        shared.stored_pages.fetch_add(1, Ordering::Relaxed);
        shared.stores.fetch_add(1, Ordering::Relaxed);
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

    /// Writes every byte of the page's contents to `page`, which may be
    /// uninitialized, and returns it initialized.
    pub fn decompress_into<'page>(
        &self,
        page: &'page mut [MaybeUninit<u8>; COMPRESSED_PAGE_BYTES],
    ) -> &'page mut [u8; COMPRESSED_PAGE_BYTES] {
        let pool = &self.pool;
        let processor = current_processor().id() as usize;
        let row = pool.decoders.get(processor).unwrap_or_else(|| {
            panic!(
                "processor {processor} has no decoder among the compressed pool's {}",
                pool.decoders.len()
            )
        });
        let mut decoder = row.lock();
        let decoded = decoder
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
        for (byte, decoded) in page.iter_mut().zip(decoded.iter()) {
            byte.write(*decoded);
        }
        pool.loads.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the loop wrote all `COMPRESSED_PAGE_BYTES` bytes, the
        // length checked above, and the two array types share a layout.
        unsafe { &mut *core::ptr::from_mut(page).cast::<[u8; COMPRESSED_PAGE_BYTES]>() }
    }
}

impl Drop for CompressedPage {
    fn drop(&mut self) {
        self.pool
            .stored_bytes
            .fetch_sub(self.bytes.len(), Ordering::Relaxed);
        self.pool.stored_pages.fetch_sub(1, Ordering::Relaxed);
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

        let mut out = [MaybeUninit::uninit(); COMPRESSED_PAGE_BYTES];
        assert_eq!(compressed.decompress_into(&mut out), &page);
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
        assert!(matches!(
            pool.compress(&text_page()),
            Err(CompressRefusal::Full { .. })
        ));
        assert_eq!(pool.stats().limit_hits, 1);
        drop(first);
        pool.compress(&text_page()).expect("room again");
    }

    #[test]
    fn the_level_is_an_operator_setting_zstd_validates() {
        let pool = CompressedPool::new(1 << 20, 1);
        assert_eq!(pool.level(), DEFAULT_COMPRESSION_LEVEL);
        pool.set_level(3).expect("zstd level 3");
        assert_eq!(pool.level(), 3);
        assert_eq!(
            pool.set_level(99),
            Err(CompressedPoolError::InvalidLevel(99))
        );
        assert_eq!(pool.level(), 3);
        let page = text_page();
        let compressed = pool.compress(&page).expect("compresses");
        let mut out = [MaybeUninit::uninit(); COMPRESSED_PAGE_BYTES];
        assert_eq!(compressed.decompress_into(&mut out), &page);
    }
}
