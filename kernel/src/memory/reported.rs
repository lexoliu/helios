//! Free-page reporting bookkeeping shared by the kernel's frame pools.
//!
//! Reporting a run of free memory to a consumer outside the kernel — a
//! virtio balloon telling its host, a migration hint — buys the host the
//! right to throw that memory away. The frames stay the guest's to
//! allocate; their *contents* do not survive. The allocator therefore
//! has to remember which free frames it has shown to someone, so the
//! next hand-out zeroes them even when the caller opted out of zeroing.
//!
//! One bit per frame is the exact answer and the only one that survives
//! a run being split across several later allocations: a range-set would
//! have to split its records, and a bounded range-set would have to drop
//! one, which is exactly the case where the guarantee matters.
//!
//! Concurrency contract: the bitmap sits behind a spin mutex taken for
//! the length of a word walk and never held across an await. It is grown
//! only from [`ReportedFrames::cover`], which runs during
//! single-processor bring-up.

extern crate alloc;

use core::future::Future;

use alloc::vec;
use alloc::vec::Vec;

use helios_hal::pmm::{PhysFrame, PhysFrameRange};
use spin::Mutex;

/// Runs a single [`visit_free_runs`] pass holds at once.
///
/// A pass takes every run it visits out of the pool for the duration of
/// the visit, so the bound is what keeps a report from emptying the pool
/// under a concurrent allocator on another processor.
pub(crate) const MAX_FREE_RUN_BATCH: usize = 32;

const BITS_PER_WORD: usize = u64::BITS as usize;

/// The frames a pool has shown to a free-page consumer.
pub(crate) struct ReportedFrames {
    bits: Mutex<FrameBitmap>,
}

impl ReportedFrames {
    pub(crate) const fn new() -> Self {
        Self {
            bits: Mutex::new(FrameBitmap::empty()),
        }
    }

    /// Grows the map to cover the byte range `[start, end)`.
    ///
    /// Called once per memory region a backend publishes, before any
    /// allocation is served from it.
    pub(crate) fn cover(&self, start: usize, end: usize) {
        if end <= start {
            return;
        }
        let first = start / PhysFrame::SIZE;
        let last = end.div_ceil(PhysFrame::SIZE);
        self.bits.lock().cover(first, last);
    }

    /// Records that `range` has been shown to a consumer.
    pub(crate) fn mark(&self, range: PhysFrameRange) {
        let mut bits = self.bits.lock();
        for frame in 0..range.frame_count {
            bits.set(range.start.index() + frame);
        }
    }

    /// Clears `range` and reports whether any of it had been shown to a
    /// consumer, and therefore has to be zeroed before it is used again.
    #[must_use]
    pub(crate) fn take(&self, range: PhysFrameRange) -> bool {
        self.take_bytes(range.start.phys_addr(), range.byte_size())
    }

    /// The byte-granular form of [`ReportedFrames::take`].
    ///
    /// A pool that serves sub-frame allocations can hand out part of a
    /// frame that was reported. Only the frames the allocation covers
    /// whole are cleared: the rest stay marked, so the next caller that
    /// lands in one is zeroed too rather than reading whatever the
    /// consumer left behind.
    #[must_use]
    pub(crate) fn take_bytes(&self, start: usize, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let mut bits = self.bits.lock();
        let covered = start.div_ceil(PhysFrame::SIZE)..(start + len) / PhysFrame::SIZE;
        let mut found = false;
        for frame in start / PhysFrame::SIZE..(start + len).div_ceil(PhysFrame::SIZE) {
            if !bits.get(frame) {
                continue;
            }
            found = true;
            if covered.contains(&frame) {
                bits.clear(frame);
            }
        }
        found
    }

    /// How many frames are currently marked.
    pub(crate) fn count(&self) -> usize {
        self.bits.lock().count()
    }
}

impl Default for ReportedFrames {
    fn default() -> Self {
        Self::new()
    }
}

/// One bit per frame over a contiguous span of frame indices.
struct FrameBitmap {
    base_frame: usize,
    frames: usize,
    words: Vec<u64>,
}

impl FrameBitmap {
    const fn empty() -> Self {
        Self {
            base_frame: 0,
            frames: 0,
            words: Vec::new(),
        }
    }

    fn cover(&mut self, first: usize, last: usize) {
        if self.frames == 0 {
            self.base_frame = first;
            self.frames = last - first;
            self.words = vec![0; self.frames.div_ceil(BITS_PER_WORD)];
            return;
        }
        let base = self.base_frame.min(first);
        let end = (self.base_frame + self.frames).max(last);
        if base == self.base_frame && end == self.base_frame + self.frames {
            return;
        }
        let mut grown = vec![0_u64; (end - base).div_ceil(BITS_PER_WORD)];
        for index in 0..self.frames {
            if word_bit(&self.words, index) {
                let moved = self.base_frame + index - base;
                grown[moved / BITS_PER_WORD] |= 1 << (moved % BITS_PER_WORD);
            }
        }
        self.base_frame = base;
        self.frames = end - base;
        self.words = grown;
    }

    fn get(&self, frame: usize) -> bool {
        self.index_of(frame)
            .is_some_and(|index| word_bit(&self.words, index))
    }

    fn set(&mut self, frame: usize) {
        if let Some(index) = self.index_of(frame) {
            self.words[index / BITS_PER_WORD] |= 1 << (index % BITS_PER_WORD);
        }
    }

    fn clear(&mut self, frame: usize) {
        if let Some(index) = self.index_of(frame) {
            self.words[index / BITS_PER_WORD] &= !(1 << (index % BITS_PER_WORD));
        }
    }

    fn count(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    fn index_of(&self, frame: usize) -> Option<usize> {
        let index = frame.checked_sub(self.base_frame)?;
        (index < self.frames).then_some(index)
    }
}

fn word_bit(words: &[u64], index: usize) -> bool {
    words[index / BITS_PER_WORD] & (1 << (index % BITS_PER_WORD)) != 0
}

/// Shows free runs of `min_frames` frames to `visit`, holding each one
/// out of the pool while it is visited.
///
/// `allocate` and `free` are the pool's own contiguous-run paths, taken
/// as closures so both kernel frame pools share this loop rather than
/// each growing their own copy. `allocate` must not itself consult the
/// reported bitmap: a report that zeroed the memory it is about to hand
/// back would pay for the whole pool on every pass.
pub(crate) async fn visit_free_runs<Allocate, Free, Visit, Visited>(
    reported: &ReportedFrames,
    min_frames: usize,
    max_runs: usize,
    mut allocate: Allocate,
    mut free: Free,
    mut visit: Visit,
) -> usize
where
    Allocate: FnMut(usize) -> Option<PhysFrameRange>,
    Free: FnMut(PhysFrameRange),
    Visit: FnMut(PhysFrameRange) -> Visited,
    Visited: Future<Output = ()>,
{
    if min_frames == 0 {
        return 0;
    }
    let bound = max_runs.min(MAX_FREE_RUN_BATCH);
    let mut held = [const { None }; MAX_FREE_RUN_BATCH];
    let mut visited = 0;
    while visited < bound {
        let Some(range) = allocate(min_frames) else {
            break;
        };
        visit(range).await;
        reported.mark(range);
        held[visited] = Some(range);
        visited += 1;
    }
    for range in held.into_iter().flatten() {
        free(range);
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::{ReportedFrames, visit_free_runs};
    use helios_hal::pmm::{PhysFrame, PhysFrameRange};

    fn range(first: usize, frames: usize) -> PhysFrameRange {
        PhysFrameRange {
            start: PhysFrame::from_index(first),
            frame_count: frames,
        }
    }

    #[test]
    fn taking_a_marked_range_reports_it_and_clears_it() {
        let reported = ReportedFrames::new();
        reported.cover(0, 64 * PhysFrame::SIZE);

        assert!(!reported.take(range(0, 4)), "nothing has been reported");
        reported.mark(range(8, 4));
        assert_eq!(reported.count(), 4);

        assert!(reported.take(range(8, 4)));
        assert_eq!(reported.count(), 0);
        assert!(
            !reported.take(range(8, 4)),
            "a range is only clobbered once per report"
        );
    }

    /// A reported run is routinely handed back in pieces, and every
    /// piece has to carry the clobbered mark that the run had.
    #[test]
    fn a_reported_run_stays_marked_where_it_was_not_taken() {
        let reported = ReportedFrames::new();
        reported.cover(0, 64 * PhysFrame::SIZE);
        reported.mark(range(16, 8));

        assert!(reported.take(range(16, 2)));
        assert_eq!(reported.count(), 6);
        assert!(reported.take(range(20, 4)));
        assert_eq!(reported.count(), 2);
        assert!(
            !reported.take(range(0, 16)),
            "frames outside the reported run were never shown to anyone"
        );
    }

    /// Backends publish their memory a region at a time, and a later
    /// region can start below the first one.
    #[test]
    fn growing_the_map_downwards_keeps_the_marks_it_had() {
        let reported = ReportedFrames::new();
        reported.cover(64 * PhysFrame::SIZE, 128 * PhysFrame::SIZE);
        reported.mark(range(70, 2));

        reported.cover(0, 64 * PhysFrame::SIZE);
        assert_eq!(reported.count(), 2);
        assert!(reported.take(range(70, 2)));
        assert_eq!(reported.count(), 0);
    }

    /// A frame outside every published region cannot be marked, and
    /// must not panic the caller that asks about it either.
    #[test]
    fn frames_outside_the_published_regions_are_never_marked() {
        let reported = ReportedFrames::new();
        reported.cover(0, 8 * PhysFrame::SIZE);
        reported.mark(range(100, 4));
        assert_eq!(reported.count(), 0);
        assert!(!reported.take(range(100, 4)));
    }

    #[test]
    fn a_pass_visits_and_releases_every_run_it_took() {
        use futures_lite::future::block_on;
        let reported = ReportedFrames::new();
        reported.cover(0, 64 * PhysFrame::SIZE);
        let mut next = 0usize;
        let mut freed = alloc::vec::Vec::new();
        let mut seen = alloc::vec::Vec::new();

        let visited = block_on(visit_free_runs(
            &reported,
            4,
            3,
            |frames| {
                let run = range(next, frames);
                next += frames;
                Some(run)
            },
            |run| freed.push(run),
            |run| {
                seen.push(run);
                core::future::ready(())
            },
        ));

        assert_eq!(visited, 3);
        assert_eq!(seen, [range(0, 4), range(4, 4), range(8, 4)]);
        assert_eq!(freed, seen, "every visited run goes back to the pool");
        assert_eq!(reported.count(), 12);
    }

    #[test]
    fn a_pool_with_no_run_that_large_visits_nothing() {
        let reported = ReportedFrames::new();
        reported.cover(0, 64 * PhysFrame::SIZE);
        let visited = futures_lite::future::block_on(visit_free_runs(
            &reported,
            512,
            4,
            |_| None,
            |_| (),
            |_| core::future::ready(()),
        ));
        assert_eq!(visited, 0);
        assert_eq!(reported.count(), 0);
    }
}
