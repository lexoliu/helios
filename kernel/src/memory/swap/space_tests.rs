//! [`SwapSpace`] over a model page-table format.
//!
//! The model is the aarch64 leaf encoding over a vector of atomic words:
//! a present leaf holds a pool frame's address, a swap entry holds its
//! token, and nothing is ever dereferenced except the frames the pool
//! handed out. What is checked is what every backend inherits from the
//! shared core: the order of a swap-out against the shootdown, the life of
//! the frame, the rollback of a refused reinstatement, the orphan queue
//! and the ageing walk.

use alloc::alloc::{Layout, alloc_zeroed};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, Once};

use helios_hal::cpu::ProcessorId;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, PageAge, PageFlags, SwapToken, VirtAddr, VirtRange};

use super::{AGE_BATCH_PAGES, SwapLeafFormat, SwapSpace};
use crate::memory::{
    MemoryOwner, ReservationTracker, allocate_user_frame_zeroed_on, allocate_user_memory_pool,
    deallocate_user_frame_on, install_user_memory_pool, user_heap_stats,
};

const PAGE: usize = PhysFrame::SIZE;
/// Where the modelled reservation starts. The model never dereferences a
/// virtual address, only the frames its leaves name.
const BASE: usize = 0x4000_0000;
const MODEL_PAGES: usize = 512;
const POOL_BYTES: usize = 4 * 1024 * 1024;
const OWNER: MemoryOwner = MemoryOwner::new(1);
const RW: PageFlags = PageFlags::READ.union(PageFlags::WRITE);

const VALID: u64 = 1 << 0;
const SWAP_MARKER: u64 = 1 << 1;
const SWAP_TOKEN_SHIFT: u32 = 2;
const ACCESSED: u64 = 1 << 10;
const FRAME_MASK: u64 = 0x0000_ffff_ffff_f000;

fn token(raw: u32) -> SwapToken {
    SwapToken::new(raw).expect("non-zero token")
}

fn page_addr(page: usize) -> VirtAddr {
    VirtAddr::new(BASE + page * PAGE)
}

fn swap_word(token: SwapToken) -> u64 {
    SWAP_MARKER | (u64::from(token.raw()) << SWAP_TOKEN_SHIFT)
}

/// The tests share one process-global pool when run as threads, and read
/// its accounting, so they take turns.
fn serialise() -> MutexGuard<'static, ()> {
    static TURN: Mutex<()> = Mutex::new(());
    static POOL: Once = Once::new();
    let guard = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    POOL.call_once(|| {
        let layout = Layout::from_size_align(POOL_BYTES, PAGE).expect("pool layout");
        // Leaked on purpose: the pool is a `&'static` for the life of the
        // test process.
        // SAFETY: the layout has a non-zero size.
        let base = unsafe { alloc_zeroed(layout) } as usize;
        assert!(base != 0, "pool backing");
        let pool = install_user_memory_pool(allocate_user_memory_pool());
        pool.initialize(&[(base, base + POOL_BYTES)]);
        pool.configure_processors(1);
    });
    guard
}

fn allocated_bytes() -> usize {
    user_heap_stats().allocated_bytes
}

struct ModelFormat {
    leaves: Vec<AtomicU64>,
    /// Written into the first and last byte of the frame by `detach`
    /// after it replaced the leaf and before it returns: a store from a
    /// processor whose translation the shootdown retires last.
    late_store: Cell<Option<u8>>,
    detach_failure: Cell<Option<AddressSpaceError>>,
    attach_failure: Cell<Option<AddressSpaceError>>,
    /// Pool bytes allocated at the moment `detach` returned.
    allocated_at_detach: Cell<usize>,
    attached_flags: Cell<Option<PageFlags>>,
    cleared_runs: RefCell<Vec<(usize, usize)>>,
}

impl ModelFormat {
    fn new() -> Self {
        Self {
            leaves: (0..MODEL_PAGES).map(|_| AtomicU64::new(0)).collect(),
            late_store: Cell::new(None),
            detach_failure: Cell::new(None),
            attach_failure: Cell::new(None),
            allocated_at_detach: Cell::new(0),
            attached_flags: Cell::new(None),
            cleared_runs: RefCell::new(Vec::new()),
        }
    }

    fn leaf(&self, page: usize) -> u64 {
        self.leaves[page].load(Ordering::Acquire)
    }

    fn set_leaf(&self, page: usize, word: u64) {
        self.leaves[page].store(word, Ordering::Release);
    }

    fn index(addr: VirtAddr) -> usize {
        (addr.raw() - BASE) / PAGE
    }

    /// Maps `page` to a fresh pool frame filled with `fill`, accessed.
    fn map_frame(&self, page: usize, fill: u8) -> NonNull<u8> {
        let frame = allocate_user_frame_zeroed_on(ProcessorId::new(0)).expect("pool frame");
        // SAFETY: the pool just handed out this whole frame.
        unsafe { core::ptr::write_bytes(frame.as_ptr(), fill, PAGE) };
        self.set_leaf(
            page,
            (frame.as_ptr() as u64 & FRAME_MASK) | VALID | ACCESSED,
        );
        frame
    }

    /// Maps `page` with no frame behind it, for tests that only look at
    /// the flags.
    fn map_flag_only(&self, page: usize) {
        self.set_leaf(page, VALID | ACCESSED);
    }

    fn frame_of(&self, page: usize) -> NonNull<u8> {
        let leaf = self.leaf(page);
        assert!(leaf & VALID != 0, "page {page} is not present");
        NonNull::new((leaf & FRAME_MASK) as *mut u8).expect("a present leaf names a frame")
    }
}

impl SwapLeafFormat for ModelFormat {
    fn swapped_token(&self, addr: VirtAddr) -> Option<SwapToken> {
        let leaf = self.leaf(Self::index(addr));
        if leaf & VALID != 0 || leaf & SWAP_MARKER == 0 {
            return None;
        }
        SwapToken::new((leaf >> SWAP_TOKEN_SHIFT) as u32)
    }

    fn detach(
        &self,
        tracker: &ReservationTracker,
        addr: VirtAddr,
        token: SwapToken,
    ) -> Result<NonNull<u8>, AddressSpaceError> {
        assert_eq!(
            tracker.swap_entry(addr).map(|entry| entry.token),
            Some(token),
            "detach runs after the tracker recorded the page as swapped"
        );
        if let Some(error) = self.detach_failure.take() {
            return Err(error);
        }
        let page = Self::index(addr);
        if self.leaf(page) & VALID == 0 {
            return Err(AddressSpaceError::NotCommitted);
        }
        let frame = self.frame_of(page);
        self.set_leaf(page, swap_word(token));
        if let Some(byte) = self.late_store.take() {
            // SAFETY: the frame is still allocated; it is the test's.
            unsafe {
                frame.as_ptr().write(byte);
                frame.as_ptr().add(PAGE - 1).write(byte);
            }
        }
        self.allocated_at_detach.set(allocated_bytes());
        Ok(frame)
    }

    fn attach(
        &self,
        tracker: &ReservationTracker,
        addr: VirtAddr,
        expected: SwapToken,
        frame: NonNull<u8>,
        flags: PageFlags,
    ) -> Result<(), AddressSpaceError> {
        assert!(
            tracker.swap_entry(addr).is_none(),
            "attach runs after the tracker counted the page committed again"
        );
        if let Some(error) = self.attach_failure.take() {
            return Err(error);
        }
        let page = Self::index(addr);
        if self.swapped_token(addr) != Some(expected) {
            return Err(AddressSpaceError::NotSwapped);
        }
        self.attached_flags.set(Some(flags));
        self.set_leaf(
            page,
            (frame.as_ptr() as u64 & FRAME_MASK) | VALID | ACCESSED,
        );
        Ok(())
    }

    fn clear_swapped(&self, addr: VirtAddr) {
        if self.swapped_token(addr).is_some() {
            self.set_leaf(Self::index(addr), 0);
        }
    }

    fn test_and_clear_accessed(&self, addr: VirtAddr) -> Option<PageAge> {
        let leaf = &self.leaves[Self::index(addr)];
        if leaf.load(Ordering::Acquire) & VALID == 0 {
            return None;
        }
        let previous = leaf.fetch_and(!ACCESSED, Ordering::AcqRel);
        Some(if previous & ACCESSED != 0 {
            PageAge::Hot
        } else {
            PageAge::Cold
        })
    }

    fn accessed_cleared(&self, start: VirtAddr, pages: usize) {
        self.cleared_runs.borrow_mut().push((start.raw(), pages));
    }
}

fn reservation() -> (ReservationTracker, VirtRange) {
    let mut tracker = ReservationTracker::new();
    let range = VirtRange::new(page_addr(0), MODEL_PAGES * PAGE);
    tracker.reserve(range);
    (tracker, range)
}

fn commit(tracker: &mut ReservationTracker, first: usize, pages: usize) -> VirtRange {
    let range = VirtRange::new(page_addr(first), pages * PAGE);
    tracker
        .record_commit(range, RW, OWNER)
        .expect("commit bookkeeping");
    range
}

fn read_page(frame: NonNull<u8>) -> Vec<u8> {
    // SAFETY: tests only read frames they hold.
    unsafe { core::slice::from_raw_parts(frame.as_ptr(), PAGE).to_vec() }
}

#[test]
fn the_page_is_read_after_the_leaf_is_replaced() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 0xAB);
    format.late_store.set(Some(0x5A));

    let mut out = vec![0_u8; PAGE];
    let flags = space
        .swap_out_page(&mut tracker, &format, page_addr(0), token(7), &mut out)
        .expect("swap out");

    assert_eq!(flags, RW);
    assert_eq!(
        (out[0], out[PAGE - 1]),
        (0x5A, 0x5A),
        "a store that lands before the shootdown completes must be in the swapped image"
    );
    assert!(out[1..PAGE - 1].iter().all(|&byte| byte == 0xAB));
    assert_eq!(format.swapped_token(page_addr(0)), Some(token(7)));
    assert_eq!(
        tracker.swap_entry(page_addr(0)).map(|entry| entry.token),
        Some(token(7))
    );
}

#[test]
fn swap_out_frees_exactly_the_frame_after_detach_returns() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    let before = allocated_bytes();
    format.map_frame(0, 1);
    assert_eq!(allocated_bytes(), before + PAGE);

    let mut out = vec![0_u8; PAGE];
    space
        .swap_out_page(&mut tracker, &format, page_addr(0), token(1), &mut out)
        .expect("swap out");

    assert_eq!(
        format.allocated_at_detach.get(),
        before + PAGE,
        "the frame is still allocated when detach returns, so the copy can read it"
    );
    assert_eq!(
        allocated_bytes(),
        before,
        "and it is freed once, afterwards"
    );
}

#[test]
fn a_failed_detach_frees_nothing_and_leaves_the_page_committed() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 1);
    let held = allocated_bytes();
    format
        .detach_failure
        .set(Some(AddressSpaceError::NotCommitted));

    let mut out = vec![0_u8; PAGE];
    assert_eq!(
        space.swap_out_page(&mut tracker, &format, page_addr(0), token(1), &mut out),
        Err(AddressSpaceError::NotCommitted)
    );

    assert_eq!(allocated_bytes(), held);
    assert!(tracker.swap_entry(page_addr(0)).is_none());
    assert_eq!(tracker.owned_resident_bytes(OWNER), PAGE as u64);
    assert!(
        format.leaf(0) & VALID != 0,
        "the leaf must still be the present one"
    );
}

#[test]
fn swap_out_checks_its_buffer_and_alignment_before_touching_anything() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 1);

    let mut short = vec![0_u8; PAGE - 1];
    assert_eq!(
        space.swap_out_page(&mut tracker, &format, page_addr(0), token(1), &mut short),
        Err(AddressSpaceError::BadPageBuffer)
    );
    let mut out = vec![0_u8; PAGE];
    assert_eq!(
        space.swap_out_page(
            &mut tracker,
            &format,
            VirtAddr::new(page_addr(0).raw() + 8),
            token(1),
            &mut out
        ),
        Err(AddressSpaceError::Misaligned)
    );
    assert!(tracker.swap_entry(page_addr(0)).is_none());
}

#[test]
fn a_page_comes_back_with_its_bytes_flags_and_token() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 0x33);
    let mut out = vec![0_u8; PAGE];
    space
        .swap_out_page(&mut tracker, &format, page_addr(0), token(9), &mut out)
        .expect("swap out");
    let after_out = allocated_bytes();

    let returned = space
        .swap_in_page(&mut tracker, &format, page_addr(0), &out)
        .expect("swap in");

    assert_eq!(returned, token(9));
    assert_eq!(allocated_bytes(), after_out + PAGE, "one frame allocated");
    assert_eq!(read_page(format.frame_of(0)), out);
    assert_eq!(format.attached_flags.get(), Some(RW));
    assert!(tracker.swap_entry(page_addr(0)).is_none());
    assert_eq!(tracker.owned_resident_bytes(OWNER), PAGE as u64);
}

#[test]
fn a_refused_reinstatement_leaves_the_page_swapped_and_returns_the_frame() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 0x44);
    let mut out = vec![0_u8; PAGE];
    space
        .swap_out_page(&mut tracker, &format, page_addr(0), token(5), &mut out)
        .expect("swap out");
    let after_out = allocated_bytes();
    format
        .attach_failure
        .set(Some(AddressSpaceError::OutOfFrames));

    assert_eq!(
        space.swap_in_page(&mut tracker, &format, page_addr(0), &out),
        Err(AddressSpaceError::OutOfFrames)
    );

    assert_eq!(
        allocated_bytes(),
        after_out,
        "the frame went back to the pool"
    );
    assert_eq!(
        tracker.swap_entry(page_addr(0)).map(|entry| entry.token),
        Some(token(5)),
        "the tracker still holds the page swapped out, under the same token"
    );
    assert_eq!(tracker.owned_resident_bytes(OWNER), 0);
    assert_eq!(format.swapped_token(page_addr(0)), Some(token(5)));

    // And the rollback is complete enough that the retry succeeds.
    space
        .swap_in_page(&mut tracker, &format, page_addr(0), &out)
        .expect("retry");
    assert_eq!(read_page(format.frame_of(0)), out);
}

#[test]
fn swapping_in_a_page_the_tracker_does_not_hold_swapped_is_refused() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 1);
    let held = allocated_bytes();

    assert_eq!(
        space.swap_in_page(&mut tracker, &format, page_addr(0), &vec![0_u8; PAGE]),
        Err(AddressSpaceError::NotSwapped)
    );
    assert_eq!(
        allocated_bytes(),
        held,
        "no frame is taken for a refused page"
    );
}

#[test]
#[should_panic(expected = "swap bookkeeping and the page table disagree")]
fn a_leaf_that_disagrees_with_the_tracker_is_an_invariant_violation() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    format.map_frame(0, 1);
    let mut out = vec![0_u8; PAGE];
    space
        .swap_out_page(&mut tracker, &format, page_addr(0), token(5), &mut out)
        .expect("swap out");
    // The leaf now names another token than the tracker's entry.
    format.set_leaf(0, swap_word(token(6)));

    let _ = space.swap_in_page(&mut tracker, &format, page_addr(0), &out);
}

fn swapped_out_pages(
    space: &SwapSpace,
    tracker: &mut ReservationTracker,
    format: &ModelFormat,
    pages: usize,
) {
    for page in 0..pages {
        format.map_frame(page, page as u8);
        let mut out = vec![0_u8; PAGE];
        space
            .swap_out_page(
                tracker,
                format,
                page_addr(page),
                token(page as u32 + 1),
                &mut out,
            )
            .expect("swap out");
    }
}

fn drained(space: &SwapSpace) -> Vec<SwapToken> {
    let mut tokens = Vec::new();
    let count = space.drain_orphaned_swap_tokens(|token| tokens.push(token));
    assert_eq!(count, tokens.len());
    tokens.sort();
    tokens
}

#[test]
fn committing_over_swapped_pages_discards_their_entries() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 3);
    swapped_out_pages(&space, &mut tracker, &format, 3);

    let over = VirtRange::new(page_addr(0), 3 * PAGE);
    space.discard(&format, tracker.take_swap_entries_in(over));
    commit(&mut tracker, 0, 3);

    assert!((0..3).all(|page| format.leaf(page) == 0));
    assert_eq!(drained(&space), [token(1), token(2), token(3)]);
    assert!(drained(&space).is_empty(), "each token drains exactly once");
}

#[test]
fn a_subrange_decommit_discards_only_the_entries_inside_it() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 3);
    swapped_out_pages(&space, &mut tracker, &format, 3);

    let middle = VirtRange::new(page_addr(1), PAGE);
    space.discard(&format, tracker.take_swap_entries_in(middle));

    assert_eq!(format.leaf(1), 0);
    assert_eq!(format.swapped_token(page_addr(0)), Some(token(1)));
    assert_eq!(format.swapped_token(page_addr(2)), Some(token(3)));
    assert_eq!(drained(&space), [token(2)]);
}

#[test]
fn releasing_a_reservation_surrenders_every_token_it_held() {
    let _turn = serialise();
    let (mut tracker, range) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 3);
    swapped_out_pages(&space, &mut tracker, &format, 3);

    let released = tracker.release(range).expect("release");
    space.discard(&format, released.swapped);

    assert!((0..3).all(|page| format.leaf(page) == 0));
    assert_eq!(drained(&space), [token(1), token(2), token(3)]);
}

#[test]
fn a_page_is_hot_for_one_scan_and_cold_for_the_next() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 3);
    (0..3).for_each(|page| format.map_flag_only(page));
    let mut ages = Vec::new();

    let visited = space.scan_committed_pages(&tracker, &format, OWNER.raw(), |_, flags, age| {
        assert_eq!(flags, RW);
        ages.push(age);
        true
    });
    assert_eq!(visited, 3);
    assert_eq!(ages, [PageAge::Hot; 3]);

    // Something touches the middle page between the two scans.
    format.set_leaf(1, VALID | ACCESSED);
    ages.clear();
    space.scan_committed_pages(&tracker, &format, OWNER.raw(), |_, _, age| {
        ages.push(age);
        true
    });
    assert_eq!(ages, [PageAge::Cold, PageAge::Hot, PageAge::Cold]);
}

#[test]
fn a_scan_skips_pages_that_are_not_present_and_other_owners() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 3);
    format.map_flag_only(0);
    format.map_flag_only(2);
    tracker
        .record_commit(VirtRange::new(page_addr(3), PAGE), RW, MemoryOwner::new(99))
        .expect("other owner");
    format.map_flag_only(3);
    let mut seen = Vec::new();

    let visited = space.scan_committed_pages(&tracker, &format, OWNER.raw(), |addr, _, _| {
        seen.push(addr);
        true
    });

    assert_eq!(visited, 2, "page 1 was never touched and is not counted");
    assert_eq!(seen, [page_addr(0), page_addr(2)]);
}

#[test]
fn cleared_flags_are_reported_in_runs_of_consecutive_pages() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    let pages = 2 * AGE_BATCH_PAGES + 44;
    commit(&mut tracker, 0, pages);
    (0..pages).for_each(|page| format.map_flag_only(page));

    space.scan_committed_pages(&tracker, &format, OWNER.raw(), |_, _, _| true);

    assert_eq!(
        *format.cleared_runs.borrow(),
        [
            (page_addr(0).raw(), AGE_BATCH_PAGES),
            (page_addr(AGE_BATCH_PAGES).raw(), AGE_BATCH_PAGES),
            (page_addr(2 * AGE_BATCH_PAGES).raw(), 44),
        ],
        "one report per full batch and one for the tail"
    );
}

#[test]
fn a_run_never_spans_a_page_that_was_not_cleared() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 4);
    (0..4).for_each(|page| format.map_flag_only(page));
    // Page 1 was already cold, so the scan clears nothing there.
    format.set_leaf(1, VALID);

    space.scan_committed_pages(&tracker, &format, OWNER.raw(), |_, _, _| true);

    assert_eq!(
        *format.cleared_runs.borrow(),
        [(page_addr(0).raw(), 1), (page_addr(2).raw(), 2),],
        "a report covers exactly the pages whose flag was cleared"
    );
}

#[test]
fn a_visitor_that_stops_ends_the_scan_and_reports_what_it_cleared() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 8);
    (0..8).for_each(|page| format.map_flag_only(page));
    let mut seen = 0;

    let visited = space.scan_committed_pages(&tracker, &format, OWNER.raw(), |_, _, _| {
        seen += 1;
        seen < 3
    });

    assert_eq!(visited, 3);
    assert_eq!(*format.cleared_runs.borrow(), [(page_addr(0).raw(), 3)]);
    assert_eq!(
        format.leaf(3) & ACCESSED,
        ACCESSED,
        "later pages are left alone"
    );
}

#[test]
fn a_substituted_page_is_never_detached_or_freed() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 1);
    // The frame stands for memory a substitution shows here: someone
    // else's, which the swap path must neither take nor free.
    let shown = format.map_frame(0, 0xC3);
    tracker
        .record_substitute(VirtRange::new(page_addr(0), PAGE))
        .expect("substitute");
    let held = allocated_bytes();
    let leaf = format.leaf(0);

    let mut out = vec![0_u8; PAGE];
    assert_eq!(
        space.swap_out_page(&mut tracker, &format, page_addr(0), token(1), &mut out),
        Err(AddressSpaceError::Substituted)
    );

    assert_eq!(
        format.leaf(0),
        leaf,
        "the leaf still shows the substitution"
    );
    assert_eq!(allocated_bytes(), held, "nothing was freed");
    assert!(tracker.swap_entry(page_addr(0)).is_none());
    assert!(out.iter().all(|&byte| byte == 0), "nothing was copied out");
    deallocate_user_frame_on(ProcessorId::new(0), shown);
}

#[test]
fn a_scan_neither_ages_nor_offers_substituted_pages() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 3);
    (0..3).for_each(|page| format.map_flag_only(page));
    tracker
        .record_substitute(VirtRange::new(page_addr(1), PAGE))
        .expect("substitute");
    let mut seen = Vec::new();

    let visited = space.scan_committed_pages(&tracker, &format, OWNER.raw(), |addr, _, _| {
        seen.push(addr);
        true
    });

    assert_eq!(visited, 2);
    assert_eq!(seen, [page_addr(0), page_addr(2)]);
    assert_eq!(
        format.leaf(1) & ACCESSED,
        ACCESSED,
        "the substituted leaf's accessed flag is not the scan's to clear"
    );
}

#[test]
fn substituting_a_swapped_page_queues_its_token_and_keeps_the_new_leaf() {
    let _turn = serialise();
    let (mut tracker, _) = reservation();
    let format = ModelFormat::new();
    let space = SwapSpace::new();
    commit(&mut tracker, 0, 2);
    swapped_out_pages(&space, &mut tracker, &format, 2);
    // What a backend does before the bookkeeping: both leaves now show
    // the substituted memory.
    (0..2).for_each(|page| format.map_flag_only(page));
    let substituted = VirtRange::new(page_addr(0), 2 * PAGE);

    space.discard(
        &format,
        tracker.record_substitute(substituted).expect("substitute"),
    );

    assert!(
        (0..2).all(|page| format.leaf(page) == VALID | ACCESSED),
        "discard leaves a leaf that is no longer a swap entry alone"
    );
    assert_eq!(drained(&space), [token(1), token(2)]);
    assert_eq!(
        space.swap_in_page(&mut tracker, &format, page_addr(0), &[0_u8; PAGE]),
        Err(AddressSpaceError::NotSwapped),
        "a substituted page has nothing to swap back in"
    );
}
