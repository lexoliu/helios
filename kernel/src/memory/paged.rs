//! A sparse byte store over user-pool frames.
//!
//! [`PagedBytes`] holds bytes on a guest's behalf in a radix tree whose
//! data pages and index nodes are all user-pool frames, so the kernel heap
//! holds the header and nothing that grows with the length. Holes cost
//! nothing: a length can be set to any `u64` without committing a frame,
//! reads of a hole return zeros, and a write commits only the pages it
//! touches, plus the index nodes on the way to them.
//!
//! # Concurrency
//!
//! - A [`PagedBytes`] has no lock of its own. Every mutation takes
//!   `&mut self`, so a file's store is reached under its owner's lock
//!   (the filesystem tree's mutex) and a snapshot's store has one writer
//!   until it is captured. A store nobody mutates is read by any number of
//!   processors through `&self` with no synchronisation: reads walk the
//!   tree and copy, and write nothing. That is why it is `Send` and `Sync`.
//! - Frame allocation and release go through the user pool: its
//!   per-processor frame slab, then its interrupt-masking lock. The lock
//!   order is the owner's lock, then the pool's, and the pool never calls
//!   into a store. Frames are released on the processor that drops them
//!   ([`current_processor`]), which a drop path may always ask.
//! - A [`FrameReservation`] is owned by one task. It takes its frames from
//!   the pool before the store is touched, so refusal is reported with the
//!   store unchanged, and what is left over goes back on drop.
//! - A [`DetachedPages`] subtree is owned by one task and shares nothing with
//!   the store it came from: truncating takes the subtrees out under the
//!   owner's lock (at most `511 * height` roots, a bound set by the tree's
//!   height and not its size), and the owner frees them after releasing
//!   it, in batches an async caller can yield between.

extern crate alloc;

use alloc::vec::Vec;
use core::alloc::Layout;
use core::ops::Range;
use core::ptr::NonNull;

use bytes::Bytes;
use helios_hal::cpu::current_processor;
use helios_hal::pmm::PhysFrame;
use thiserror::Error;

use super::UserMemoryPool;
use crate::ProgramOutOfMemory;

/// Bytes in a data page, which is one pool frame.
const PAGE: usize = PhysFrame::SIZE;
/// `log2(PAGE)`.
const PAGE_SHIFT: u32 = PAGE.trailing_zeros();
/// Children of an index node: one frame of 8-byte entries.
const FANOUT: usize = PAGE / size_of::<Option<Frame>>();
/// `log2(FANOUT)`.
const FANOUT_SHIFT: u32 = FANOUT.trailing_zeros();

const _: () = {
    assert!(PAGE.is_power_of_two(), "a frame is a power of two");
    assert!(
        size_of::<Option<Frame>>() == size_of::<usize>(),
        "an absent child must be one zero word"
    );
    assert!(FANOUT == 512, "the page store is a 512-way radix tree");
};

/// One pool frame the store owns.
///
/// An absent frame is `None`, which is the all-zero word, so a zeroed
/// frame is an index node with no children.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Frame(NonNull<u8>);

/// An index node: the children of one subtree, absent ones `None`.
type Node = [Option<Frame>; FANOUT];

impl Frame {
    /// # Safety
    ///
    /// The frame is an index node, and nothing writes it while the
    /// returned reference lives.
    unsafe fn node<'a>(self) -> &'a Node {
        // SAFETY: a frame is `PAGE`-aligned and `PAGE` long, which is
        // exactly `Node`, and every bit pattern is a valid `Node`.
        unsafe { &*self.0.as_ptr().cast::<Node>() }
    }

    /// # Safety
    ///
    /// The frame is an index node, and nothing else reaches it while the
    /// returned reference lives.
    unsafe fn node_mut<'a>(self) -> &'a mut Node {
        // SAFETY: as `node`, with exclusivity from the caller.
        unsafe { &mut *self.0.as_ptr().cast::<Node>() }
    }

    /// # Safety
    ///
    /// The frame is a data page, and nothing writes it while the returned
    /// reference lives.
    unsafe fn data<'a>(self) -> &'a [u8; PAGE] {
        // SAFETY: a frame is `PAGE` bytes and the caller says it is data.
        unsafe { &*self.0.as_ptr().cast::<[u8; PAGE]>() }
    }

    /// # Safety
    ///
    /// The frame is a data page, and nothing else reaches it while the
    /// returned reference lives.
    unsafe fn data_mut<'a>(self) -> &'a mut [u8; PAGE] {
        // SAFETY: as `data`, with exclusivity from the caller.
        unsafe { &mut *self.0.as_ptr().cast::<[u8; PAGE]>() }
    }

    fn zero(self) {
        // SAFETY: the frame is `PAGE` bytes the caller owns.
        unsafe { self.0.as_ptr().write_bytes(0, PAGE) };
    }
}

const FRAME_LAYOUT: Layout = match Layout::from_size_align(PAGE, PAGE) {
    Ok(layout) => layout,
    Err(_) => panic!("a frame is a valid layout"),
};

fn free_frame(pool: &UserMemoryPool, frame: Frame) {
    pool.deallocate_bytes_on(current_processor(), frame.0, FRAME_LAYOUT);
}

/// Pages one node at `level` covers: a data page is level 0.
const fn pages_covered(level: u8) -> u64 {
    1 << (FANOUT_SHIFT * level as u32)
}

/// The shortest height whose root covers page `last`.
fn height_for(last: u64) -> u8 {
    let mut height = 0;
    while pages_covered(height) <= last {
        height += 1;
    }
    height
}

/// Which child of a node at `level` holds `page`.
fn child_index(page: u64, level: u8) -> usize {
    ((page >> (FANOUT_SHIFT * u32::from(level - 1))) as usize) & (FANOUT - 1)
}

/// The user pool could not supply the frames a mutation needs.
///
/// Nothing was changed: reservations are taken before the store is
/// touched, and what a failed one had taken is already back in the pool.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("the user pool could not supply {needed} frames for the page store: {source}")]
pub struct FrameShortage {
    /// Frames the reservation asked for.
    pub needed: u64,
    /// What the pool answered for the frame it refused.
    #[source]
    pub source: ProgramOutOfMemory,
}

/// Frames taken from the pool ahead of a mutation.
///
/// Holding the frames first is what lets [`PagedBytes::write`] and
/// [`PagedBytes::commit`] be infallible: the pool is asked once, up front,
/// and the tree is changed only from what was granted. The frames are
/// chained through their own first word, so a reservation costs the kernel
/// heap nothing however many it holds. Their contents are undefined until
/// the store takes one. Whatever the store did not take goes back to the
/// pool when the reservation drops.
pub struct FrameReservation {
    pool: &'static UserMemoryPool,
    head: Option<Frame>,
    available: u64,
}

// SAFETY: the reservation exclusively owns every frame on its chain, and
// the pool it returns them to is `Sync`.
unsafe impl Send for FrameReservation {}

impl FrameReservation {
    /// Takes `frames` frames from `pool`, or none at all.
    pub fn take(pool: &'static UserMemoryPool, frames: u64) -> Result<Self, FrameShortage> {
        let mut reservation = Self {
            pool,
            head: None,
            available: 0,
        };
        for _ in 0..frames {
            let (ptr, _) = pool
                .allocate_uninit_on(current_processor(), FRAME_LAYOUT)
                .map_err(|source| FrameShortage {
                    needed: frames,
                    source,
                })?;
            reservation.push(Frame(ptr));
        }
        Ok(reservation)
    }

    /// Frames still held.
    pub const fn available(&self) -> u64 {
        self.available
    }

    fn push(&mut self, frame: Frame) {
        // SAFETY: the frame is ours and `PAGE` bytes long, so its first
        // word holds the link.
        unsafe { frame.0.as_ptr().cast::<Option<Frame>>().write(self.head) };
        self.head = Some(frame);
        self.available += 1;
    }

    /// One frame with undefined contents.
    ///
    /// Panics when the reservation is empty: a mutation was given fewer
    /// frames than [`PagedBytes::frames_to_commit`] said it needs.
    fn pop(&mut self) -> Frame {
        let frame = self
            .head
            .expect("the reservation covers PagedBytes::frames_to_commit");
        // SAFETY: `push` wrote the link word of every frame on the chain.
        self.head = unsafe { frame.0.as_ptr().cast::<Option<Frame>>().read() };
        self.available -= 1;
        frame
    }

    fn pop_zeroed(&mut self) -> Frame {
        let frame = self.pop();
        frame.zero();
        frame
    }
}

impl Drop for FrameReservation {
    fn drop(&mut self) {
        while self.head.is_some() {
            free_frame(self.pool, self.pop());
        }
    }
}

/// Subtrees taken out of a [`PagedBytes`] by a shrink, still holding
/// their frames.
///
/// Dropping frees whatever is left. An async caller frees in bounded
/// steps with [`Self::free_batch`] and yields between them, so releasing a
/// large file does not hold a processor.
pub struct DetachedPages {
    pool: &'static UserMemoryPool,
    /// Subtree roots and their levels. Their number is bounded by the
    /// height of the tree they came from, and by 511 per level, so this is
    /// small however much the subtrees hold.
    pending: Vec<(Frame, u8)>,
    /// Frames still to free.
    frames: u64,
}

// SAFETY: a `DetachedPages` exclusively owns the subtrees it holds.
unsafe impl Send for DetachedPages {}

impl DetachedPages {
    const fn new(pool: &'static UserMemoryPool) -> Self {
        Self {
            pool,
            pending: Vec::new(),
            frames: 0,
        }
    }

    /// Adds the subtree rooted at `root`, which holds `frames` frames.
    fn push(&mut self, root: Frame, level: u8, frames: u64) {
        self.pending.push((root, level));
        self.frames += frames;
    }

    /// Frames still to free.
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    /// Whether nothing is left to free.
    pub const fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Frees at most `max_frames` frames and answers how many it freed.
    pub fn free_batch(&mut self, max_frames: usize) -> usize {
        let mut freed = 0;
        while freed < max_frames {
            let Some((frame, level)) = self.pending.pop() else {
                break;
            };
            if level > 0 {
                // SAFETY: an index node of a subtree nothing else reaches.
                let node = unsafe { frame.node() };
                self.pending
                    .extend(node.iter().flatten().map(|child| (*child, level - 1)));
            }
            free_frame(self.pool, frame);
            freed += 1;
        }
        self.frames -= freed as u64;
        freed
    }
}

impl Drop for DetachedPages {
    fn drop(&mut self) {
        self.free_batch(usize::MAX);
    }
}

/// Bytes held in user-pool frames on a guest's behalf: a sparse radix
/// tree whose data pages and index nodes are all frames, so the kernel
/// heap holds this header and nothing that grows with the length.
///
/// An index node is one frame of 512 children, absent ones empty. Height 0
/// is a single data page and height `h` covers `512^h` pages. The height
/// grows when a write or commit reaches past the covered range, with the
/// old root becoming child 0 of a new one; it never grows on
/// [`Self::set_len`]. Absent pages read as zeros, or as the matching bytes
/// of `base` where the store was made over a read-only backing.
///
/// Mutations take `&mut self` and need frames from a [`FrameReservation`]
/// taken first; see the module docs for the concurrency contract.
pub struct PagedBytes {
    pool: &'static UserMemoryPool,
    /// The logical length: what `stat` reports, and where reads stop.
    len: u64,
    /// The index node at `height`, or the single data page at height 0.
    root: Option<Frame>,
    height: u8,
    /// Frames committed, data and index.
    frames: u64,
    /// Read-only backing for absent pages below `base.len()`: a boot file
    /// copied up shares its bytes until a page is written.
    base: Bytes,
}

// SAFETY: the store exclusively owns its frames, mutates only through
// `&mut self`, and has no interior mutability, so a shared reference only
// reads.
unsafe impl Send for PagedBytes {}
// SAFETY: see `Send`.
unsafe impl Sync for PagedBytes {}

impl PagedBytes {
    /// An empty store.
    pub const fn new(pool: &'static UserMemoryPool) -> Self {
        Self::with_base(pool, Bytes::new())
    }

    /// A store that reads as `base` until pages are written over it.
    pub const fn with_base(pool: &'static UserMemoryPool, base: Bytes) -> Self {
        Self {
            pool,
            len: base.len() as u64,
            root: None,
            height: 0,
            frames: 0,
            base,
        }
    }

    /// The logical length in bytes.
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the logical length is zero.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Frames committed, data and index.
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    #[cfg(test)]
    const fn height(&self) -> u8 {
        self.height
    }

    /// Frames a write or commit of `range` would take: the data pages in
    /// it that are absent, the index nodes they need, and the nodes a
    /// taller tree adds.
    pub fn frames_to_commit(&self, range: Range<u64>) -> u64 {
        if range.start >= range.end {
            return 0;
        }
        let first = range.start >> PAGE_SHIFT;
        let last = (range.end - 1) >> PAGE_SHIFT;
        self.grown_cost(height_for(last).max(self.height), first, last)
    }

    /// Frames needed to populate pages `first..=last` of a tree of height
    /// `level`, where a tree taller than this one has new nodes along its
    /// left edge with the old root at the bottom of them.
    fn grown_cost(&self, level: u8, first: u64, last: u64) -> u64 {
        match self.root {
            // The whole tree is new.
            None => absent_cost(level, 0, first, last),
            Some(root) if level == self.height => present_cost(root, level, 0, first, last),
            // A new node above the root. Its child 0 always exists, as the
            // next level of the left edge down to the old root, whether or
            // not the range reaches it; its other children are absent.
            Some(_) => {
                let span = pages_covered(level - 1);
                // A range that runs past this node continues under a
                // sibling, which the level above counts.
                let last_child = (last / span).min(FANOUT as u64 - 1);
                let others = ((first / span).max(1)..=last_child)
                    .map(|index| absent_cost(level - 1, index * span, first, last))
                    .sum::<u64>();
                1 + self.grown_cost(level - 1, first, last) + others
            }
        }
    }

    /// Copies the bytes at `at` into `dst`, up to the end of the store,
    /// and answers how many it copied. Holes and the part of a page past
    /// the backing read as zeros. Never allocates.
    pub fn read(&self, at: u64, dst: &mut [u8]) -> usize {
        if at >= self.len {
            return 0;
        }
        let total = dst
            .len()
            .min(usize::try_from(self.len - at).unwrap_or(usize::MAX));
        let mut done = 0;
        while done < total {
            let position = at + done as u64;
            let offset = (position & (PAGE as u64 - 1)) as usize;
            let run = (PAGE - offset).min(total - done);
            let out = &mut dst[done..done + run];
            match self.leaf(position >> PAGE_SHIFT) {
                // SAFETY: a data page, and `&self` means nothing writes it.
                Some(page) => out.copy_from_slice(&unsafe { page.data() }[offset..offset + run]),
                None => {
                    let backed = self
                        .base
                        .len()
                        .saturating_sub(usize::try_from(position).unwrap_or(usize::MAX))
                        .min(run);
                    if backed > 0 {
                        let from = position as usize;
                        out[..backed].copy_from_slice(&self.base[from..from + backed]);
                    }
                    out[backed..].fill(0);
                }
            }
            done += run;
        }
        total
    }

    /// The data page `page`, if committed.
    fn leaf(&self, page: u64) -> Option<Frame> {
        let mut node = self.root?;
        if page >= pages_covered(self.height) {
            return None;
        }
        for level in (1..=self.height).rev() {
            // SAFETY: an index node, read under `&self`.
            node = unsafe { node.node() }[child_index(page, level)]?;
        }
        Some(node)
    }

    /// Writes `len` bytes at `at`, extending the length to cover them.
    ///
    /// `fill` is called over consecutive runs, none crossing a page, whose
    /// lengths sum to `len`, in order; it must write every byte it is
    /// given, because a run that covers a whole new page is handed the
    /// frame as it came from the pool. Pages the write does not cover are
    /// zeros, or the backing's bytes below its length. The gap between the
    /// old length and `at` stays a hole.
    ///
    /// `frames` must hold at least [`Self::frames_to_commit`] of the range
    /// as of this call, and `at + len` must fit a `u64`.
    pub fn write(
        &mut self,
        at: u64,
        len: usize,
        frames: &mut FrameReservation,
        mut fill: impl FnMut(&mut [u8]),
    ) {
        if len == 0 {
            return;
        }
        let end = at
            .checked_add(len as u64)
            .expect("the caller bounds a write to the u64 range");
        debug_assert!(
            frames.available() >= self.frames_to_commit(at..end),
            "the reservation covers PagedBytes::frames_to_commit"
        );
        self.grow_height(height_for((end - 1) >> PAGE_SHIFT), frames);
        let mut created = 0;
        let mut done = 0;
        while done < len {
            let position = at + done as u64;
            let offset = (position & (PAGE as u64 - 1)) as usize;
            let run = (PAGE - offset).min(len - done);
            let page = ensure_leaf(
                &mut self.root,
                self.height,
                &self.base,
                position >> PAGE_SHIFT,
                offset == 0 && run == PAGE,
                frames,
                &mut created,
            );
            // SAFETY: a data page owned through `&mut self`.
            fill(&mut unsafe { page.data_mut() }[offset..offset + run]);
            done += run;
        }
        self.frames += created;
        self.len = self.len.max(end);
    }

    /// Commits every page of `range` that is absent, zeroed or copied from
    /// the backing, so that a later write inside it takes no frame.
    /// `posix_fallocate`'s storage half: the length is the caller's to set.
    ///
    /// `frames` must hold at least [`Self::frames_to_commit`] of the range.
    pub fn commit(&mut self, range: Range<u64>, frames: &mut FrameReservation) {
        if range.start >= range.end {
            return;
        }
        debug_assert!(
            frames.available() >= self.frames_to_commit(range.clone()),
            "the reservation covers PagedBytes::frames_to_commit"
        );
        let first = range.start >> PAGE_SHIFT;
        let last = (range.end - 1) >> PAGE_SHIFT;
        self.grow_height(height_for(last), frames);
        let mut created = 0;
        for page in first..=last {
            ensure_leaf(
                &mut self.root,
                self.height,
                &self.base,
                page,
                false,
                frames,
                &mut created,
            );
        }
        self.frames += created;
    }

    /// Raises the root to at least `target`.
    fn grow_height(&mut self, target: u8, frames: &mut FrameReservation) {
        if self.root.is_none() {
            // Nothing to wrap: the root is made at this height on demand.
            self.height = self.height.max(target);
            return;
        }
        while self.height < target {
            let top = frames.pop_zeroed();
            // SAFETY: a fresh index node nothing else reaches.
            let children = unsafe { top.node_mut() };
            children[0] = self.root;
            self.root = Some(top);
            self.height += 1;
            self.frames += 1;
        }
    }

    /// Sets the length. Growing commits nothing and costs O(1). Shrinking
    /// zeroes the cut page past the new end, so a later extension reads
    /// zeros, clamps the backing, and takes every subtree beyond the cut
    /// out of the tree and returns it for the caller to free outside its
    /// lock.
    pub fn set_len(&mut self, len: u64) -> DetachedPages {
        let mut detached = DetachedPages::new(self.pool);
        if len >= self.len {
            self.len = len;
            return detached;
        }
        self.len = len;
        if let Ok(len) = usize::try_from(len) {
            self.base.truncate(len);
        }
        if len & (PAGE as u64 - 1) != 0
            && let Some(page) = self.leaf(len >> PAGE_SHIFT)
        {
            let from = (len & (PAGE as u64 - 1)) as usize;
            // SAFETY: a data page owned through `&mut self`.
            let data = unsafe { page.data_mut() };
            data[from..].fill(0);
        }
        let Some(root) = self.root else {
            return detached;
        };
        let keep = len.div_ceil(PAGE as u64);
        if keep == 0 {
            self.root = None;
            detached.push(root, self.height, self.frames);
            self.height = 0;
            self.frames = 0;
            return detached;
        }
        if self.height > 0 {
            let before = detached.frames;
            detach_beyond(root, self.height, 0, keep, &mut detached);
            self.frames -= detached.frames - before;
        }
        self.collapse_root();
        detached
    }

    /// Drops the levels a shrink left unused: an empty root goes, and a
    /// root with only child 0 gives way to it.
    fn collapse_root(&mut self) {
        while self.height > 0
            && let Some(root) = self.root
        {
            // SAFETY: an index node owned through `&mut self`.
            let node = unsafe { root.node() };
            if node[1..].iter().any(Option::is_some) {
                break;
            }
            self.root = node[0];
            if self.root.is_none() {
                self.height = 0;
            } else {
                self.height -= 1;
            }
            free_frame(self.pool, root);
            self.frames -= 1;
        }
    }
}

impl Drop for PagedBytes {
    fn drop(&mut self) {
        if let Some(root) = self.root.take() {
            let mut whole = DetachedPages::new(self.pool);
            whole.push(root, self.height, self.frames);
        }
    }
}

/// Frames to populate pages `first..=last` under the existing `node` at
/// `level`, whose first page is `base`.
fn present_cost(node: Frame, level: u8, base: u64, first: u64, last: u64) -> u64 {
    if level == 0 {
        return 0;
    }
    let span = pages_covered(level - 1);
    let low = first.max(base);
    let high = last.min(base + pages_covered(level) - 1);
    if low > high {
        return 0;
    }
    // SAFETY: an index node, read under the caller's `&self`.
    let node = unsafe { node.node() };
    (((low - base) / span)..=((high - base) / span))
        .map(|index| {
            let child_base = base + index * span;
            match node[index as usize] {
                Some(child) => present_cost(child, level - 1, child_base, first, last),
                None => absent_cost(level - 1, child_base, first, last),
            }
        })
        .sum()
}

/// Frames to populate pages `first..=last` under an absent node at
/// `level` whose first page is `base`: the node, and the nodes and pages
/// below it that the range reaches, counted level by level.
fn absent_cost(level: u8, base: u64, first: u64, last: u64) -> u64 {
    let low = first.max(base);
    let high = last.min(base + pages_covered(level) - 1);
    if low > high {
        return 0;
    }
    (0..=level)
        .map(|step| {
            let shift = FANOUT_SHIFT * u32::from(step);
            (high >> shift) - (low >> shift) + 1
        })
        .sum()
}

/// The data page `page`, made from `frames` along with the index nodes
/// above it when absent; `created` counts what it took.
///
/// A page written whole is not initialised, because the caller overwrites
/// all of it. Any other new page is zeroed, with the backing's bytes
/// copied in where the backing reaches.
fn ensure_leaf(
    root: &mut Option<Frame>,
    height: u8,
    base: &Bytes,
    page: u64,
    whole: bool,
    frames: &mut FrameReservation,
    created: &mut u64,
) -> Frame {
    let mut slot = root;
    let mut level = height;
    loop {
        if level == 0 {
            if slot.is_none() {
                let frame = if whole {
                    frames.pop()
                } else {
                    let frame = frames.pop_zeroed();
                    let start = usize::try_from(page << PAGE_SHIFT).unwrap_or(usize::MAX);
                    if start < base.len() {
                        let backed = (base.len() - start).min(PAGE);
                        // SAFETY: a fresh data page nothing else reaches.
                        let data = unsafe { frame.data_mut() };
                        data[..backed].copy_from_slice(&base[start..start + backed]);
                    }
                    frame
                };
                *created += 1;
                *slot = Some(frame);
            }
            return slot.expect("the page was just made");
        }
        let node = match *slot {
            Some(node) => node,
            None => {
                let node = frames.pop_zeroed();
                *created += 1;
                *slot = Some(node);
                node
            }
        };
        // SAFETY: an index node owned through the caller's `&mut`.
        slot = &mut unsafe { node.node_mut() }[child_index(page, level)];
        level -= 1;
    }
}

/// Takes every subtree of `node` that lies wholly at or beyond page
/// `keep` out into `detached`, descending into the one child the cut runs
/// through, and that child too when the cut leaves it empty, so a shrink
/// keeps no index node that holds nothing. `node` is an index node at
/// `level` whose first page is `base`. Answers whether `node` is left with
/// no child.
fn detach_beyond(
    node: Frame,
    level: u8,
    base: u64,
    keep: u64,
    detached: &mut DetachedPages,
) -> bool {
    let span = pages_covered(level - 1);
    // SAFETY: an index node owned through the caller's `&mut`.
    let children = unsafe { node.node_mut() };
    let first = (keep.saturating_sub(base) / span) as usize;
    for (index, slot) in children.iter_mut().enumerate().skip(first) {
        let Some(child) = *slot else { continue };
        let child_base = base + index as u64 * span;
        if child_base >= keep {
            *slot = None;
            let frames = subtree_frames(child, level - 1);
            detached.push(child, level - 1, frames);
        } else if level > 1 && detach_beyond(child, level - 1, child_base, keep, detached) {
            *slot = None;
            detached.push(child, level - 1, 1);
        }
    }
    children.iter().all(Option::is_none)
}

/// Frames in the subtree rooted at `node`, counted from the index nodes
/// alone: no data page is touched.
fn subtree_frames(node: Frame, level: u8) -> u64 {
    match level {
        0 => 1,
        1 => {
            // SAFETY: an index node of a subtree being detached.
            1 + unsafe { node.node() }.iter().flatten().count() as u64
        }
        _ => {
            // SAFETY: as above.
            1 + unsafe { node.node() }
                .iter()
                .flatten()
                .map(|child| subtree_frames(*child, level - 1))
                .sum::<u64>()
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;
    use alloc::vec;

    use super::*;

    /// Garbage the pool's memory holds before any test touches it, so a
    /// page that should be zero and is not shows at once.
    const GARBAGE: u8 = 0xAA;
    const FRAME: usize = PAGE;

    /// A pool over `frames` frames of leaked host memory filled with
    /// garbage.
    fn pool(frames: usize) -> &'static UserMemoryPool {
        let bytes = frames * PAGE;
        let layout = Layout::from_size_align(bytes, PAGE).expect("pool layout");
        // SAFETY: a non-zero layout.
        let start = unsafe { alloc::alloc::alloc(layout) };
        assert!(!start.is_null(), "host allocation for the test pool failed");
        // SAFETY: `bytes` bytes just allocated.
        unsafe { start.write_bytes(GARBAGE, bytes) };
        let pool = Box::leak(Box::new(UserMemoryPool::empty()));
        pool.initialize(&[(start as usize, start as usize + bytes)]);
        pool.configure_processors(1);
        pool
    }

    fn allocated(pool: &UserMemoryPool) -> usize {
        pool.stats().allocated_bytes
    }

    /// Reserves for `range` and writes `bytes` at `at`.
    fn put(store: &mut PagedBytes, at: u64, bytes: &[u8]) {
        let range = at..at + bytes.len() as u64;
        let mut frames =
            FrameReservation::take(store.pool, store.frames_to_commit(range)).expect("frames");
        let mut rest = bytes;
        store.write(at, bytes.len(), &mut frames, |run| {
            let (head, tail) = rest.split_at(run.len());
            run.copy_from_slice(head);
            rest = tail;
        });
        assert!(rest.is_empty(), "fill covered every byte");
    }

    fn get(store: &PagedBytes, at: u64, len: usize) -> Vec<u8> {
        let mut out = vec![GARBAGE; len];
        let read = store.read(at, &mut out);
        out.truncate(read);
        out
    }

    /// T3.1: a length is a number, not storage.
    #[test]
    fn growing_the_length_commits_nothing() {
        let pool = pool(256);
        let before = allocated(pool);
        let mut store = PagedBytes::new(pool);
        assert!(store.set_len(1 << 40).is_empty());
        assert_eq!(store.len(), 1 << 40);
        assert_eq!(store.frames(), 0);
        assert_eq!(allocated(pool), before);
        assert_eq!(get(&store, (1 << 40) - 8, 64), vec![0; 8]);
    }

    /// T3.2: one byte at the end of a 1 TiB hole is a data page and four
    /// index levels, and the height does not move until that write.
    #[test]
    fn one_byte_at_a_terabyte_commits_five_frames() {
        let pool = pool(256);
        let before = allocated(pool);
        let mut store = PagedBytes::new(pool);
        store.set_len(1 << 40);
        assert_eq!(store.height(), 0, "a length does not grow the tree");
        assert_eq!(store.frames_to_commit((1 << 40) - 1..1 << 40), 5);

        put(&mut store, (1 << 40) - 1, &[0x7e]);
        assert_eq!(store.frames(), 5);
        assert_eq!(store.height(), 4);
        assert_eq!(allocated(pool) - before, 5 * FRAME);
        assert_eq!(get(&store, (1 << 40) - 2, 2), [0, 0x7e]);
        assert_eq!(store.len(), 1 << 40);
    }

    /// The count a write is told it needs is the count it takes, across
    /// an empty store, a store the write makes taller, and a store it
    /// fills in.
    #[test]
    fn frames_to_commit_is_what_a_write_takes() {
        let pool = pool(2048);
        let mut store = PagedBytes::new(pool);
        let ranges: [Range<u64>; 7] = [
            0..1,
            100..PAGE as u64 * 3,
            PAGE as u64 * 600..PAGE as u64 * 601,
            PAGE as u64 * 5..PAGE as u64 * 700,
            (1 << 30)..(1 << 30) + 9000,
            PAGE as u64 * 2..PAGE as u64 * 4,
            (1 << 45)..(1 << 45) + 1,
        ];
        for range in ranges {
            let before = store.frames();
            let promised = store.frames_to_commit(range.clone());
            let bytes = vec![0x11; (range.end - range.start) as usize];
            put(&mut store, range.start, &bytes);
            assert_eq!(store.frames() - before, promised, "{range:?}");
            assert_eq!(store.frames_to_commit(range.clone()), 0, "{range:?}");
            assert_eq!(get(&store, range.start, bytes.len()), bytes);
        }
    }

    /// A write that makes the tree two levels taller and crosses from one
    /// subtree of the new root into the next is promised what it takes:
    /// the pages under the sibling are counted once, by the level that
    /// owns them.
    #[test]
    fn a_range_crossing_subtrees_of_a_taller_tree_is_counted_once() {
        let pool = pool(2048);
        let mut store = PagedBytes::new(pool);
        put(&mut store, 0, &[1]);
        assert_eq!(store.height(), 0);

        // The last page under the root's child 0 at height 3, and the 513
        // pages after it under child 1.
        let boundary = pages_covered(2);
        let range = PAGE as u64 * (boundary - 1)..PAGE as u64 * (boundary + 513);
        // 514 data pages; above them three level-1 nodes (one before the
        // boundary, two after), two level-2 nodes and the root, and the
        // level-1 node that takes the old root as its child 0.
        let promised = store.frames_to_commit(range.clone());
        assert_eq!(promised, 514 + 3 + 2 + 1 + 1);
        let before = store.frames();
        let bytes = vec![0x44; (range.end - range.start) as usize];
        put(&mut store, range.start, &bytes);
        assert_eq!(store.height(), 3);
        assert_eq!(store.frames() - before, promised);
        assert_eq!(get(&store, 0, 1), [1]);
    }

    /// A shrink that cuts a page out from under index nodes holding
    /// nothing else takes those nodes too: no frame stays committed for an
    /// empty subtree.
    #[test]
    fn a_shrink_keeps_no_empty_index_node() {
        let pool = pool(256);
        let before = allocated(pool);
        let mut store = PagedBytes::new(pool);
        put(&mut store, 0, &[1]);
        // One page far out: three index levels above it, sharing only the
        // root with page 0's path.
        let far = PAGE as u64 * (pages_covered(2) + 1);
        put(&mut store, far, &[2]);
        assert_eq!(store.height(), 3);
        assert_eq!(store.frames(), 1 + 2 + 1 + 1 + 2);

        // The cut runs through the far page's level-1 node, one page
        // before it, where nothing is committed.
        let detached = store.set_len(far);
        assert_eq!(detached.frames(), 3, "the page and both nodes above it");
        drop(detached);
        assert_eq!(store.height(), 0, "only page 0 is left");
        assert_eq!(store.frames(), 1);
        assert_eq!(allocated(pool) - before, FRAME);
        assert_eq!(get(&store, 0, 1), [1]);
    }

    /// T3.3: holes read zeros, and a partial write to a fresh page leaves
    /// the rest of that page zero, whatever the frame held before.
    #[test]
    fn holes_and_partial_pages_read_zeros() {
        let pool = pool(256);
        let mut store = PagedBytes::new(pool);
        put(&mut store, PAGE as u64 * 5 + 100, &[1, 2, 3, 4]);
        assert_eq!(store.len(), PAGE as u64 * 5 + 104);

        let page = get(&store, PAGE as u64 * 5, PAGE);
        assert_eq!(page.len(), 104);
        assert!(page[..100].iter().all(|byte| *byte == 0));
        assert_eq!(page[100..], [1, 2, 3, 4]);
        store.set_len(PAGE as u64 * 6);
        let page = get(&store, PAGE as u64 * 5, PAGE);
        assert_eq!(page.len(), PAGE);
        assert!(page[104..].iter().all(|byte| *byte == 0));
        assert!(get(&store, 0, PAGE * 5).iter().all(|byte| *byte == 0));
        assert_eq!(store.read(store.len(), &mut [0; 4]), 0);
    }

    /// A write that covers whole pages hands `fill` page-sized runs, in
    /// order, and a ragged one splits at the page edges.
    #[test]
    fn fill_runs_follow_the_page_edges() {
        let pool = pool(256);
        let mut store = PagedBytes::new(pool);
        let at = PAGE as u64 - 10;
        let len = PAGE * 2 + 30;
        let mut frames =
            FrameReservation::take(pool, store.frames_to_commit(at..at + len as u64)).unwrap();
        let mut runs = Vec::new();
        store.write(at, len, &mut frames, |run| {
            runs.push(run.len());
            run.fill(0x5c);
        });
        assert_eq!(runs, [10, PAGE, PAGE, 20]);
        assert_eq!(frames.available(), 0);
        assert!(get(&store, at, len).iter().all(|byte| *byte == 0x5c));
        assert!(get(&store, 0, at as usize).iter().all(|byte| *byte == 0));
    }

    /// T3.4: after a shrink to the middle of a page, growing again reads
    /// zeros past the cut, not what the page held.
    #[test]
    fn shrinking_mid_page_then_growing_reads_zeros_past_the_cut() {
        let pool = pool(256);
        let mut store = PagedBytes::new(pool);
        put(&mut store, 0, &vec![0x11; PAGE * 3]);
        assert_eq!(store.frames(), 4);

        let cut = PAGE as u64 + 100;
        let detached = store.set_len(cut);
        assert_eq!(detached.frames(), 1, "the third page");
        drop(detached);
        assert_eq!(store.frames(), 3);
        store.set_len(PAGE as u64 * 3);

        let read = get(&store, 0, PAGE * 3);
        assert_eq!(read.len(), PAGE * 3);
        assert!(read[..cut as usize].iter().all(|byte| *byte == 0x11));
        assert!(read[cut as usize..].iter().all(|byte| *byte == 0));
    }

    /// T3.5: everything the store committed goes back, through drop, a
    /// detached subtree and the batched path alike, and a shrink that
    /// leaves one child collapses the levels above it.
    #[test]
    fn dropping_and_detaching_return_the_pool_to_its_baseline() {
        let pool = pool(2048);
        let before = allocated(pool);

        let mut store = PagedBytes::new(pool);
        put(&mut store, 0, &vec![0x22; PAGE * 700]);
        put(&mut store, PAGE as u64 * 5000, &[1]);
        assert_eq!(store.height(), 2);
        let committed = store.frames();
        assert_eq!(allocated(pool) - before, committed as usize * FRAME);

        let mut detached = store.set_len(PAGE as u64 * 10);
        assert_eq!(store.height(), 1, "the root keeps only child 0");
        assert_eq!(store.frames(), 11, "a node and ten pages");
        assert_eq!(detached.frames() + store.frames() + 1, committed);
        let mut batches = 0;
        let mut freed = 0;
        while !detached.is_empty() {
            let step = detached.free_batch(100);
            assert!(step > 0, "a detached subtree with frames left frees some");
            freed += step as u64;
            batches += 1;
        }
        assert!(batches > 1, "a large subtree frees in steps");
        assert_eq!(freed + store.frames() + 1, committed);
        assert_eq!(allocated(pool) - before, store.frames() as usize * FRAME);

        let detached = store.set_len(0);
        assert_eq!(detached.frames(), 11);
        assert_eq!(store.frames(), 0);
        drop(detached);
        assert_eq!(allocated(pool), before);

        put(&mut store, 0, &vec![0x33; PAGE * 40]);
        drop(store);
        assert_eq!(allocated(pool), before);
    }

    /// T3.6: a pool that cannot serve the reservation refuses it whole and
    /// keeps nothing; what a write does not take goes back.
    #[test]
    fn a_short_pool_refuses_the_reservation_and_surplus_returns() {
        let pool = pool(64);
        let mut held = Vec::new();
        while let Ok((ptr, _)) = pool.allocate_uninit_on(current_processor(), FRAME_LAYOUT) {
            held.push(Frame(ptr));
        }
        let spare: usize = 6;
        for _ in 0..spare {
            free_frame(pool, held.pop().expect("the pool had frames"));
        }
        let before = allocated(pool);

        let mut store = PagedBytes::new(pool);
        let needed = store.frames_to_commit(0..PAGE as u64 * 8);
        assert_eq!(needed, 9);
        let refused = FrameReservation::take(pool, needed).err().expect("refused");
        assert_eq!(refused.needed, 9);
        assert_eq!(allocated(pool), before, "the partial reservation went back");
        assert_eq!(store.frames(), 0);
        assert_eq!(store.len(), 0);
        assert_eq!(get(&store, 0, 8), Vec::<u8>::new());

        // The six that remain serve a write of one page and its node, and
        // the surplus goes back when the reservation drops.
        let mut frames = FrameReservation::take(pool, spare as u64).expect("fits");
        assert_eq!(frames.available(), spare as u64);
        assert_eq!(allocated(pool) - before, spare * FRAME);
        let at = PAGE as u64 * 600;
        assert_eq!(store.frames_to_commit(at..at + 1), 3);
        store.write(at, 1, &mut frames, |run| run.fill(9));
        assert_eq!(frames.available(), 3);
        drop(frames);
        assert_eq!(allocated(pool) - before, 3 * FRAME);
        drop(store);
        assert_eq!(allocated(pool), before);
        for frame in held {
            free_frame(pool, frame);
        }
    }

    /// T3.7: a store over a backing reads it, commits only the pages it is
    /// written to, and keeps the backing's bytes in them.
    #[test]
    fn a_backed_store_copies_only_the_pages_it_writes() {
        let pool = pool(256);
        let before = allocated(pool);
        let content: Vec<u8> = (0..PAGE * 3 + 100)
            .map(|index| (index % 251) as u8)
            .collect();
        let mut store = PagedBytes::with_base(pool, Bytes::from(content.clone()));
        assert_eq!(store.len(), content.len() as u64);
        assert_eq!(store.frames(), 0);
        assert_eq!(allocated(pool), before);
        assert_eq!(get(&store, 0, content.len() + 50), content);

        put(&mut store, PAGE as u64 + 7, &[0xff]);
        // A data page and the node above it.
        assert_eq!(store.frames(), 2);
        assert_eq!(allocated(pool) - before, 2 * FRAME);
        let mut expected = content.clone();
        expected[PAGE + 7] = 0xff;
        assert_eq!(get(&store, 0, content.len()), expected);

        // A write past the backing's end takes the partial last page from
        // it too, and zeros after it.
        put(&mut store, PAGE as u64 * 3 + 100, &[0xee]);
        let tail = get(&store, PAGE as u64 * 3, 102);
        assert_eq!(tail[..100], content[PAGE * 3..]);
        assert_eq!(tail[100..], [0xee]);

        // Shrinking clamps the backing, so growing reads zeros.
        store.set_len(PAGE as u64 + 10);
        store.set_len(PAGE as u64 * 4);
        let shrunk = get(&store, 0, PAGE * 4);
        assert_eq!(shrunk[..PAGE + 7], expected[..PAGE + 7]);
        assert_eq!(shrunk[PAGE + 7], 0xff);
        assert_eq!(shrunk[PAGE + 8..PAGE + 10], expected[PAGE + 8..PAGE + 10]);
        assert!(shrunk[PAGE + 10..].iter().all(|byte| *byte == 0));
    }

    /// Committing a range takes its pages once and leaves the length and
    /// the bytes alone.
    #[test]
    fn commit_populates_pages_without_changing_the_length() {
        let pool = pool(256);
        let before = allocated(pool);
        let mut store = PagedBytes::new(pool);
        let range = 0..PAGE as u64 * 4 + 1;
        let needed = store.frames_to_commit(range.clone());
        assert_eq!(needed, 6, "five pages and one index node");
        let mut frames = FrameReservation::take(pool, needed).unwrap();
        store.commit(range.clone(), &mut frames);
        assert_eq!(frames.available(), 0);
        assert_eq!(store.frames(), needed);
        assert_eq!(store.len(), 0);
        assert_eq!(store.frames_to_commit(range), 0);
        store.set_len(PAGE as u64 * 5);
        assert!(get(&store, 0, PAGE * 5).iter().all(|byte| *byte == 0));
        drop(store);
        assert_eq!(allocated(pool), before);
    }
}
