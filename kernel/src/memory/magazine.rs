//! Per-processor caches of small kernel-heap blocks.
//!
//! # Why
//!
//! Every kernel allocation on every processor takes one lock. The lock
//! is short and interrupt-safe (see [`super::irq_safe`]), but it is
//! still one word every processor writes, so a four-processor guest
//! serialises its whole allocation stream through it. Replacing the
//! allocator behind that lock made instance startup 1.48x faster
//! (#246) and bounding the user pool's free path another 1.17x (#248);
//! neither touched the serialisation itself, which is what this does.
//!
//! A magazine holds blocks of one size class, already carved out of the
//! shared heap, on the processor that will hand them out. An allocation
//! that hits one takes no lock and touches no line another processor
//! writes.
//!
//! # Concurrency contract
//!
//! Every operation on a magazine runs on the processor that owns it,
//! with that processor's interrupts masked. Nothing arrives from
//! another processor, so there is no cross-processor path to make
//! lock-free: the owner is the only writer, and the mask is against its
//! own interrupt handler, which allocates and frees like any other
//! code.
//!
//! A block allocated on one processor and freed on another lands in the
//! freeing processor's magazine. That is not an ownership violation,
//! because a block is interchangeable with every other block of its
//! class: the heap it came from serves and accepts any of them from any
//! processor. It does mean a magazine can accumulate blocks a different
//! processor allocated, which the flush below bounds.
//!
//! Counters are the one thing another processor reads. Each magazine
//! keeps its own on its own cache line, written by the owner alone with
//! plain loads and stores rather than read-modify-writes, and a stats
//! read sums them. `live_bytes` is signed because a processor that frees
//! more than it allocated has a negative share of a total that is still
//! correct.
//!
//! # What is cached
//!
//! Small blocks whose alignment the class already guarantees. A layout
//! wanting more alignment than [`CACHED_ALIGN`], or more bytes than the
//! largest class, goes to the shared heap unchanged.
//!
//! Cached blocks are allocated and freed at their class's canonical
//! layout, never at the caller's, so the heap always sees the same
//! layout for a block it served. The caller's layout still decides the
//! class, and `dealloc` is handed the layout `alloc` was called with, so
//! the two agree by construction.
//!
//! Per-size-class metrics turn the cache off while they are enabled
//! (see [`Magazines::set_bypassed`]): they exist to say exactly what the
//! heap was asked for, and a cache that answers without reaching the
//! heap would make them a report about refills.

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};

use alloc::boxed::Box;
use alloc::vec::Vec;
use crossbeam_utils::CachePadded;
use helios_hal::critical_section::with_local_interrupts_masked;
use spin::Once;

/// The largest alignment a cached block satisfies.
///
/// Every class size is a multiple of it, so a block of any class is
/// aligned to it, and a layout asking for more goes to the heap.
pub(crate) const CACHED_ALIGN: usize = 16;

/// Size classes, `CACHED_ALIGN << i` bytes each: 16 through 512.
///
/// The upper end is where a per-processor cache stops paying: above it
/// the allocation is rare enough per unit of work that the lock it
/// takes is not what the workload is waiting on, and the memory a
/// magazine would hold idle on every processor starts to matter.
const CLASS_COUNT: usize = 6;

/// Blocks one class holds before a free flushes half of them back.
const CAPACITY: usize = 64;

/// Blocks one refill takes from the heap under a single lock.
const REFILL: usize = 16;

/// Blocks one flush returns to the heap under a single lock.
const FLUSH: usize = CAPACITY / 2;

/// A class's canonical layout: the layout the heap sees for every block
/// of that class, whatever the caller asked for.
fn canonical_layout(class: usize) -> Layout {
    let size = CACHED_ALIGN << class;
    // Safety: the size is a non-zero multiple of the alignment, and the
    // alignment is a power of two.
    unsafe { Layout::from_size_align_unchecked(size, CACHED_ALIGN) }
}

/// The class serving `layout`, or `None` when the heap must serve it
/// directly.
fn class_of(layout: Layout) -> Option<usize> {
    if layout.align() > CACHED_ALIGN {
        return None;
    }
    let size = layout.size().max(CACHED_ALIGN);
    let largest = CACHED_ALIGN << (CLASS_COUNT - 1);
    if size > largest {
        return None;
    }
    // The class whose size first covers the request.
    let slots = size.div_ceil(CACHED_ALIGN);
    Some(usize::try_from(slots.next_power_of_two().trailing_zeros()).unwrap_or(CLASS_COUNT))
        .filter(|class| *class < CLASS_COUNT)
}

/// One class's blocks, threaded through their own first word.
///
/// The list is a LIFO so a refill's last block is the next allocation's
/// first: it is the one most likely still in this processor's cache.
struct ClassCache {
    head: *mut u8,
    depth: usize,
}

impl ClassCache {
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            depth: 0,
        }
    }

    /// # Safety
    ///
    /// `block` must be a live allocation of this class's canonical
    /// layout that nothing else refers to, and the caller must own this
    /// magazine's processor with interrupts masked.
    unsafe fn push(&mut self, block: *mut u8) {
        // Safety: a class block is at least `CACHED_ALIGN` bytes and
        // aligned to it, so its first word holds a pointer.
        unsafe { block.cast::<*mut u8>().write(self.head) };
        self.head = block;
        self.depth += 1;
    }

    /// # Safety
    ///
    /// The caller must own this magazine's processor with interrupts
    /// masked.
    unsafe fn pop(&mut self) -> *mut u8 {
        let block = self.head;
        if block.is_null() {
            return block;
        }
        // Safety: every block on the list was written by `push`.
        self.head = unsafe { block.cast::<*mut u8>().read() };
        self.depth -= 1;
        block
    }
}

/// Counters one processor writes and every processor may read.
///
/// The owner writes with a load and a store rather than a
/// read-modify-write: it is the only writer, and the line is its own.
#[derive(Default)]
struct Counters {
    allocation_count: AtomicU64,
    deallocation_count: AtomicU64,
    total_allocation_bytes: AtomicU64,
    total_deallocation_bytes: AtomicU64,
    live_bytes: AtomicI64,
    cached_bytes: AtomicUsize,
}

impl Counters {
    fn add(value: &AtomicU64, delta: u64) {
        value.store(
            value.load(Ordering::Relaxed).wrapping_add(delta),
            Ordering::Relaxed,
        );
    }

    fn record_alloc(&self, size: usize, cached_delta: isize) {
        Self::add(&self.allocation_count, 1);
        Self::add(&self.total_allocation_bytes, size as u64);
        self.live_bytes.store(
            self.live_bytes
                .load(Ordering::Relaxed)
                .wrapping_add(size as i64),
            Ordering::Relaxed,
        );
        self.adjust_cached(cached_delta);
    }

    fn record_dealloc(&self, size: usize, cached_delta: isize) {
        Self::add(&self.deallocation_count, 1);
        Self::add(&self.total_deallocation_bytes, size as u64);
        self.live_bytes.store(
            self.live_bytes
                .load(Ordering::Relaxed)
                .wrapping_sub(size as i64),
            Ordering::Relaxed,
        );
        self.adjust_cached(cached_delta);
    }

    fn adjust_cached(&self, delta: isize) {
        let current = self.cached_bytes.load(Ordering::Relaxed);
        let next = current.wrapping_add_signed(delta);
        self.cached_bytes.store(next, Ordering::Relaxed);
    }
}

/// One processor's magazine.
struct Magazine {
    /// Owner-only, interrupts masked. See the module contract.
    classes: UnsafeCell<[ClassCache; CLASS_COUNT]>,
    counters: Counters,
}

// Safety: `classes` is reached only through `Magazines::with_owned`,
// which runs on the owning processor with its interrupts masked, and
// nothing hands out a reference that outlives that closure.
unsafe impl Send for Magazine {}
unsafe impl Sync for Magazine {}

impl Magazine {
    fn new() -> Self {
        Self {
            classes: UnsafeCell::new([const { ClassCache::new() }; CLASS_COUNT]),
            counters: Counters::default(),
        }
    }

    /// Runs `act` on this processor's lists with its interrupts masked.
    ///
    /// # Safety
    ///
    /// The caller must have reached this magazine through
    /// [`Magazines::magazine`], which is what makes it this
    /// processor's own.
    unsafe fn with_lists<R>(&self, act: impl FnOnce(&mut [ClassCache; CLASS_COUNT]) -> R) -> R {
        with_local_interrupts_masked(|| {
            // Safety: the owning processor, and its interrupt handler
            // cannot run until this returns, so no other reference to
            // the lists can exist.
            act(unsafe { &mut *self.classes.get() })
        })
    }

    fn pop(&self, class: usize) -> Option<*mut u8> {
        // Safety: see `with_lists`.
        let block = unsafe { self.with_lists(|classes| classes[class].pop()) };
        (!block.is_null()).then_some(block)
    }

    /// Adds `block` and answers whether the class is now over capacity.
    ///
    /// # Safety
    ///
    /// `block` must be a live allocation of the class's canonical
    /// layout that nothing else refers to.
    unsafe fn push(&self, class: usize, block: *mut u8) -> bool {
        // Safety: see `with_lists`; the block promise is the caller's.
        unsafe {
            self.with_lists(|classes| {
                classes[class].push(block);
                classes[class].depth > CAPACITY
            })
        }
    }

    fn push_all(&self, class: usize, blocks: &[*mut u8]) {
        // Safety: see `with_lists`. Every block came from a refill of
        // this class's canonical layout.
        unsafe {
            self.with_lists(|classes| {
                for block in blocks {
                    classes[class].push(*block);
                }
            });
        }
    }

    fn pop_many(&self, class: usize, blocks: &mut [*mut u8; FLUSH]) -> usize {
        // Safety: see `with_lists`.
        unsafe {
            self.with_lists(|classes| {
                let mut taken = 0;
                while taken < blocks.len() {
                    let block = classes[class].pop();
                    if block.is_null() {
                        break;
                    }
                    blocks[taken] = block;
                    taken += 1;
                }
                taken
            })
        }
    }
}

/// A snapshot of what the magazines have served and are holding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MagazineStats {
    pub(crate) allocation_count: u64,
    pub(crate) deallocation_count: u64,
    pub(crate) total_allocation_bytes: u64,
    pub(crate) total_deallocation_bytes: u64,
    pub(crate) live_bytes: i64,
    pub(crate) cached_bytes: usize,
}

/// The per-processor caches in front of the kernel heap.
pub(crate) struct Magazines {
    processors: Once<Box<[CachePadded<Magazine>]>>,
    /// Set while per-size-class metrics are on, which turns the cache
    /// off so those counts describe what the heap was asked for.
    bypassed: AtomicBool,
}

impl Magazines {
    pub(crate) const fn new() -> Self {
        Self {
            processors: Once::new(),
            bypassed: AtomicBool::new(false),
        }
    }

    /// Gives every processor a magazine.
    ///
    /// Called once the kernel heap can allocate and the processor count
    /// is known. Until then every allocation goes to the heap, which is
    /// also what a single-processor machine gets nothing from changing.
    pub(crate) fn initialize(&self, processors: usize) {
        self.processors.call_once(|| {
            let mut all = Vec::with_capacity(processors);
            all.resize_with(processors, || CachePadded::new(Magazine::new()));
            all.into_boxed_slice()
        });
    }

    pub(crate) fn set_bypassed(&self, bypassed: bool) {
        self.bypassed.store(bypassed, Ordering::Release);
    }

    fn magazine(&self) -> Option<&Magazine> {
        if self.bypassed.load(Ordering::Acquire) {
            return None;
        }
        let processors = self.processors.get()?;
        let index = usize::from(helios_hal::cpu::current_processor().id());
        processors.get(index).map(|padded| &**padded)
    }

    /// Serves `layout` from this processor's magazine, or answers null.
    ///
    /// `refill` fills `blocks` with allocations of the canonical layout
    /// it is handed and answers how many it wrote; it may write fewer
    /// than the array holds, and zero when the heap cannot serve the
    /// class at all. It runs with interrupts unmasked, because it takes
    /// the heap lock, which masks them itself for its own critical
    /// section: the mask here covers only the list.
    pub(crate) fn allocate<Refill>(&self, layout: Layout, refill: Refill) -> *mut u8
    where
        Refill: FnOnce(Layout, &mut [*mut u8; REFILL]) -> usize,
    {
        let Some(class) = class_of(layout) else {
            return ptr::null_mut();
        };
        let Some(magazine) = self.magazine() else {
            return ptr::null_mut();
        };
        let block_size = CACHED_ALIGN << class;
        if let Some(block) = magazine.pop(class) {
            magazine.counters.record_alloc(
                layout.size(),
                -isize::try_from(block_size).unwrap_or(isize::MAX),
            );
            return block;
        }

        let mut blocks = [ptr::null_mut(); REFILL];
        let taken = refill(canonical_layout(class), &mut blocks);
        if taken == 0 {
            return ptr::null_mut();
        }
        // An interrupt may have refilled this class in the window
        // above; its blocks and these are interchangeable, so both
        // stay and the next free flushes whatever is over capacity.
        magazine.push_all(class, &blocks[..taken]);
        let Some(block) = magazine.pop(class) else {
            return ptr::null_mut();
        };
        let cached = isize::try_from((taken - 1) * block_size).unwrap_or(isize::MAX);
        magazine.counters.record_alloc(layout.size(), cached);
        block
    }

    /// Takes `block` back into this processor's magazine.
    ///
    /// Answers `false` when the block does not belong to a class or no
    /// magazine exists yet, in which case the caller returns it to the
    /// heap. `flush` is handed a class's canonical layout and the
    /// blocks over capacity, with interrupts unmasked for the same
    /// reason as `refill`.
    pub(crate) fn deallocate<Flush>(&self, block: *mut u8, layout: Layout, flush: Flush) -> bool
    where
        Flush: FnOnce(Layout, &[*mut u8]),
    {
        let Some(class) = class_of(layout) else {
            return false;
        };
        let Some(magazine) = self.magazine() else {
            return false;
        };
        let block_size = CACHED_ALIGN << class;
        // Safety: the caller promises `block` is a live allocation this
        // magazine's class served, which is the canonical layout.
        let over_capacity = unsafe { magazine.push(class, block) };
        if !over_capacity {
            magazine.counters.record_dealloc(
                layout.size(),
                isize::try_from(block_size).unwrap_or(isize::MAX),
            );
            return true;
        }

        let mut returning = [ptr::null_mut(); FLUSH];
        let flushed = magazine.pop_many(class, &mut returning);
        if flushed != 0 {
            flush(canonical_layout(class), &returning[..flushed]);
        }
        let cached = isize::try_from(block_size).unwrap_or(isize::MAX)
            - isize::try_from(flushed * block_size).unwrap_or(isize::MAX);
        magazine.counters.record_dealloc(layout.size(), cached);
        true
    }

    /// What every magazine has served and is holding.
    pub(crate) fn stats(&self) -> MagazineStats {
        let Some(processors) = self.processors.get() else {
            return MagazineStats::default();
        };
        let mut stats = MagazineStats::default();
        for magazine in processors.iter() {
            let counters = &magazine.counters;
            stats.allocation_count = stats
                .allocation_count
                .wrapping_add(counters.allocation_count.load(Ordering::Relaxed));
            stats.deallocation_count = stats
                .deallocation_count
                .wrapping_add(counters.deallocation_count.load(Ordering::Relaxed));
            stats.total_allocation_bytes = stats
                .total_allocation_bytes
                .wrapping_add(counters.total_allocation_bytes.load(Ordering::Relaxed));
            stats.total_deallocation_bytes = stats
                .total_deallocation_bytes
                .wrapping_add(counters.total_deallocation_bytes.load(Ordering::Relaxed));
            stats.live_bytes = stats
                .live_bytes
                .wrapping_add(counters.live_bytes.load(Ordering::Relaxed));
            stats.cached_bytes = stats
                .cached_bytes
                .wrapping_add(counters.cached_bytes.load(Ordering::Relaxed));
        }
        stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks for a test's refills, taken from the host allocator so
    /// they are real memory the list can thread itself through.
    struct Blocks {
        layout: Layout,
        held: Vec<*mut u8>,
    }

    impl Blocks {
        fn new(layout: Layout) -> Self {
            Self {
                layout,
                held: Vec::new(),
            }
        }

        fn take(&mut self, count: usize, into: &mut [*mut u8]) -> usize {
            let taken = count.min(into.len());
            for slot in into.iter_mut().take(taken) {
                // Safety: a class layout is non-zero sized.
                let block = unsafe { alloc::alloc::alloc(self.layout) };
                assert!(!block.is_null(), "test host allocator ran out");
                self.held.push(block);
                *slot = block;
            }
            taken
        }
    }

    impl Drop for Blocks {
        fn drop(&mut self) {
            for block in self.held.drain(..) {
                // Safety: every block came from `take` under `layout`.
                unsafe { alloc::alloc::dealloc(block, self.layout) };
            }
        }
    }

    fn magazines() -> Magazines {
        let magazines = Magazines::new();
        magazines.initialize(1);
        magazines
    }

    #[test]
    fn classes_cover_small_layouts_and_stop() {
        assert_eq!(class_of(Layout::from_size_align(1, 1).unwrap()), Some(0));
        assert_eq!(class_of(Layout::from_size_align(16, 16).unwrap()), Some(0));
        assert_eq!(class_of(Layout::from_size_align(17, 1).unwrap()), Some(1));
        assert_eq!(class_of(Layout::from_size_align(32, 8).unwrap()), Some(1));
        assert_eq!(class_of(Layout::from_size_align(33, 1).unwrap()), Some(2));
        assert_eq!(class_of(Layout::from_size_align(512, 16).unwrap()), Some(5));
        assert_eq!(class_of(Layout::from_size_align(513, 1).unwrap()), None);
        assert_eq!(class_of(Layout::from_size_align(16, 32).unwrap()), None);
    }

    #[test]
    fn a_class_layout_covers_every_request_it_serves() {
        for size in 1..=(CACHED_ALIGN << (CLASS_COUNT - 1)) {
            let layout = Layout::from_size_align(size, 1).unwrap();
            let class = class_of(layout).expect("a small layout has a class");
            let canonical = canonical_layout(class);
            assert!(
                canonical.size() >= size,
                "class {class} serves {size} bytes from {}",
                canonical.size()
            );
            assert!(canonical.align() >= layout.align());
        }
    }

    #[test]
    fn a_refill_serves_the_allocations_that_follow_it() {
        let magazines = magazines();
        let layout = Layout::from_size_align(24, 8).unwrap();
        let mut blocks = Blocks::new(canonical_layout(class_of(layout).unwrap()));
        let mut refills = 0;

        let first = magazines.allocate(layout, |canonical, into| {
            refills += 1;
            assert_eq!(canonical, canonical_layout(1));
            blocks.take(REFILL, into)
        });
        assert!(!first.is_null());
        assert_eq!(refills, 1);

        for _ in 1..REFILL {
            let block =
                magazines.allocate(layout, |_, _| unreachable!("the refill still has blocks"));
            assert!(!block.is_null());
        }
        let empty = magazines.allocate(layout, |_canonical, into| {
            refills += 1;
            blocks.take(1, into)
        });
        assert!(!empty.is_null());
        assert_eq!(refills, 2, "the class only refills once it is empty");
    }

    #[test]
    fn a_heap_that_cannot_serve_the_class_answers_null() {
        let magazines = magazines();
        let layout = Layout::from_size_align(64, 8).unwrap();
        let block = magazines.allocate(layout, |_, _| 0);
        assert!(block.is_null(), "an empty refill leaves the caller to grow");
    }

    #[test]
    fn a_layout_no_class_serves_is_left_to_the_heap() {
        let magazines = magazines();
        let large = Layout::from_size_align(4096, 8).unwrap();
        assert!(
            magazines
                .allocate(large, |_, _| unreachable!("no class refills"))
                .is_null()
        );
        assert!(!magazines.deallocate(ptr::dangling_mut(), large, |_, _| {
            unreachable!("no class flushes")
        }));
    }

    #[test]
    fn frees_past_capacity_go_back_to_the_heap() {
        let magazines = magazines();
        let layout = Layout::from_size_align(48, 8).unwrap();
        let class = class_of(layout).unwrap();
        let mut blocks = Blocks::new(canonical_layout(class));
        let mut owned = [ptr::null_mut(); CAPACITY + 1];
        assert_eq!(blocks.take(CAPACITY + 1, &mut owned), CAPACITY + 1);

        let mut flushed = 0;
        for (index, block) in owned.iter().enumerate() {
            let took = magazines.deallocate(*block, layout, |canonical, returning| {
                assert_eq!(canonical, canonical_layout(class));
                flushed += returning.len();
            });
            assert!(took);
            if index < CAPACITY {
                assert_eq!(flushed, 0, "capacity is not reached until it is exceeded");
            }
        }
        assert_eq!(flushed, FLUSH, "the class returns half of itself at once");
    }

    #[test]
    fn stats_report_what_was_served_and_what_is_held() {
        let magazines = magazines();
        let layout = Layout::from_size_align(24, 8).unwrap();
        let class = class_of(layout).unwrap();
        let mut blocks = Blocks::new(canonical_layout(class));

        let block = magazines.allocate(layout, |_, into| blocks.take(REFILL, into));
        assert!(!block.is_null());
        let served = magazines.stats();
        assert_eq!(served.allocation_count, 1);
        assert_eq!(served.total_allocation_bytes, 24);
        assert_eq!(served.live_bytes, 24);
        assert_eq!(
            served.cached_bytes,
            (REFILL - 1) * (CACHED_ALIGN << class),
            "the refill's remainder is held, the served block is not"
        );

        assert!(magazines.deallocate(block, layout, |_, _| unreachable!("below capacity")));
        let returned = magazines.stats();
        assert_eq!(returned.deallocation_count, 1);
        assert_eq!(returned.live_bytes, 0);
        assert_eq!(returned.cached_bytes, REFILL * (CACHED_ALIGN << class));
    }

    #[test]
    fn bypassing_leaves_every_allocation_to_the_heap() {
        let magazines = magazines();
        magazines.set_bypassed(true);
        let layout = Layout::from_size_align(24, 8).unwrap();
        assert!(
            magazines
                .allocate(layout, |_, _| unreachable!("bypassed"))
                .is_null()
        );
        assert!(!magazines.deallocate(ptr::dangling_mut(), layout, |_, _| {
            unreachable!("bypassed")
        }));
    }

    #[test]
    fn magazines_that_do_not_exist_yet_serve_nothing() {
        let magazines = Magazines::new();
        let layout = Layout::from_size_align(24, 8).unwrap();
        assert!(
            magazines
                .allocate(layout, |_, _| unreachable!("no processors"))
                .is_null()
        );
        assert_eq!(magazines.stats(), MagazineStats::default());
    }
}
