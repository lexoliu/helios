//! The page index a native user address space keeps beside its hardware
//! translation tables, shared by every backend that implements
//! `helios_hal::native`.
//!
//! The index maps each mapped user page to the user-memory frame behind it,
//! or to none for a page whose frame was discarded, and the access it was
//! mapped with. It answers the lookups the contract's operations need, the
//! copies of [`helios_hal::native::UserSpace`] and the fault path, in
//! `O(log n)` per range rather than a scan of every page, and copies each
//! run of physically adjacent frames in one copy.
//!
//! # Concurrency
//!
//! The index is single-owner: a space holds it under the lock that also
//! serialises its page-table writes, and every method runs with that lock
//! held. Frames are reached through the kernel's direct map, never through a
//! user address.
//!
//! [`NativeSpace`] keeps the slice count `helios_hal::native` describes in
//! a cache line of its own: every processor entering or leaving a slice of
//! the space writes it and every removal reads it, so it shares no line
//! with the lock word removals and copies spin on. A removal holds the lock
//! from its first table write through its shootdown, so no copy, fault
//! resolution or other change of the space sees a page whose translation
//! is gone from the tables but not yet from every processor.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering, fence};

use crossbeam_utils::CachePadded;
use helios_hal::native::{Mapping, NativeError, UserSpace};
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{PageFlags, VirtAddr, VirtRange};
use spin::Mutex;

use crate::memory::{allocate_user_frame_zeroed_on, deallocate_user_frame_on};

const PAGE: u64 = PhysFrame::SIZE as u64;

/// The hardware translation tables of one native user address space: the
/// part of [`NativeSpace`] each backend supplies.
///
/// # Safety
///
/// `install` maps exactly the frame it is given at the page it is given,
/// reachable from user mode with no more access than `flags`, `protect`
/// grants no more than its `flags`, and neither touches the kernel's half
/// of the address space. `invalidate` returns only once no processor holds
/// a translation of the range older than the call.
pub unsafe trait UserTranslation: Send + 'static {
    /// Maps page `base`, which is not mapped, to the frame at `physical`.
    /// The page goes from not present to present, so no processor holds a
    /// translation of it and no invalidation is needed.
    fn install(&mut self, base: u64, physical: u64, flags: PageFlags) -> Result<(), NativeError>;

    /// Removes the translation of present page `base`. Processors may still
    /// hold it: the caller runs [`Self::invalidate`] over the page, when a
    /// slice may hold it, before the frame is freed, mapped elsewhere alone,
    /// or the removal is reported done.
    fn uninstall(&mut self, base: u64);

    /// Replaces the access of present page `base` with `flags`, keeping its
    /// frame. A narrowing is invalidated by the caller as `uninstall` is.
    fn protect(&mut self, base: u64, flags: PageFlags);

    /// Invalidates every translation of `[first, last)` on this processor
    /// and every other, and returns once each has acknowledged: the
    /// backend's TLB shootdown. Runs in task context with interrupts
    /// enabled.
    fn invalidate(&self, first: u64, last: u64);

    /// The physical address of a user frame, from its direct-map address.
    fn physical(&self, frame: NonNull<u8>) -> u64;
}

/// A native user address space: the page index and the backend's
/// translation tables behind one spin lock, held for a table write, a
/// copy, or a removal and its shootdown, never across a slice or an
/// `.await`, and the count of slices running it. It is the [`UserSpace`]
/// every backend publishes; see `helios_hal::native` for the concurrency
/// contract it keeps.
///
/// Both live in one block the space owns, so the space itself has the
/// alignment of a pointer wherever it is held: the padded counter's
/// cache-line alignment (128 bytes on x86-64 and AArch64) would otherwise
/// pass to every future that holds a space, past what the executor's task
/// arena places.
pub struct NativeSpace<Tables> {
    shared: Box<Shared<Tables>>,
}

const _: () = assert!(align_of::<NativeSpace<()>>() == align_of::<usize>());

struct Shared<Tables> {
    state: Mutex<SpaceState<Tables>>,
    /// Slices of this space running now, on any processor. Every processor
    /// that enters or leaves a slice of the space writes it.
    slices: CachePadded<AtomicUsize>,
}

struct SpaceState<Tables> {
    pages: NativePages,
    tables: Tables,
}

/// One running slice of a [`NativeSpace`], counted from before it reads
/// the space's root until it is dropped. A backend drops it only once the
/// slice's exit has left no translation of the space on its processor.
pub struct NativeSlice<'a, Tables> {
    space: &'a NativeSpace<Tables>,
}

impl<Tables: UserTranslation> NativeSlice<'_, Tables> {
    /// Runs `operation` on the translation tables under the space's lock,
    /// as a slice's entry does to read the root it loads.
    pub fn with_tables<Output>(&self, operation: impl FnOnce(&mut Tables) -> Output) -> Output {
        operation(&mut self.space.shared.state.lock().tables)
    }
}

impl<Tables> Drop for NativeSlice<'_, Tables> {
    fn drop(&mut self) {
        self.space.shared.slices.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a change took out of the tables: the frames to free once its
/// shootdown is done, and the page span `[first, last)` it removed or
/// narrowed translations in, empty when it touched none.
struct Removed {
    frames: Vec<NonNull<u8>>,
    first: u64,
    last: u64,
}

impl Removed {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            first: u64::MAX,
            last: 0,
        }
    }

    fn touch(&mut self, base: u64) {
        self.first = self.first.min(base);
        self.last = self.last.max(base + PAGE);
    }
}

/// An installation that ran out of memory or tables part way, with the
/// pages it had installed already taken back out.
struct Failed {
    error: NativeError,
    removed: Removed,
}

impl<Tables: UserTranslation> NativeSpace<Tables> {
    /// An empty space whose user range is `[start, end)`.
    pub fn new(tables: Tables, start: u64, end: u64) -> Self {
        Self {
            shared: Box::new(Shared {
                state: Mutex::new(SpaceState {
                    pages: NativePages::new(start, end),
                    tables,
                }),
                slices: CachePadded::new(AtomicUsize::new(0)),
            }),
        }
    }

    /// Counts a slice of this space as running. The caller takes it before
    /// it reads the root it loads, so a removal that misses the count wrote
    /// its tables before that slice walks them.
    pub fn begin_slice(&self) -> NativeSlice<'_, Tables> {
        self.shared.slices.fetch_add(1, Ordering::SeqCst);
        NativeSlice { space: self }
    }

    /// Completes a change: invalidates what it removed or narrowed wherever
    /// a slice may hold it, then frees the frames it took out.
    fn complete(&self, tables: &Tables, removed: Removed) {
        if removed.first < removed.last {
            // Pairs with `begin_slice`: either this load sees a slice that
            // is starting, or that slice counts itself after it and the root
            // it loads walks the tables as written above. A slice that has
            // ended left nothing behind on its processor.
            fence(Ordering::SeqCst);
            if self.shared.slices.load(Ordering::SeqCst) != 0 {
                tables.invalidate(removed.first, removed.last);
            }
        }
        let processor = helios_hal::cpu::current_processor();
        for frame in removed.frames {
            deallocate_user_frame_on(processor, frame);
        }
    }
}

impl<Tables: UserTranslation> SpaceState<Tables> {
    /// Installs zero-filled pages over `[first, last)`, which has no
    /// mapping. A failure takes back what it installed.
    fn install(&mut self, first: u64, last: u64, flags: PageFlags) -> Result<(), Failed> {
        let processor = helios_hal::cpu::current_processor();
        for base in (first..last).step_by(PAGE as usize) {
            let Ok(frame) = allocate_user_frame_zeroed_on(processor) else {
                return Err(self.fail(first, base, NativeError::OutOfMemory));
            };
            let physical = self.tables.physical(frame);
            if let Err(error) = self.tables.install(base, physical, flags) {
                deallocate_user_frame_on(processor, frame);
                return Err(self.fail(first, base, error));
            }
            self.pages.insert(
                base,
                NativePage {
                    frame: Some(frame),
                    flags,
                },
            );
        }
        Ok(())
    }

    /// Takes the pages of `[first, last)` out of the index and the tables.
    /// Another thread's slice may have reached any of them, so their frames
    /// wait for the shootdown.
    fn take(&mut self, first: u64, last: u64) -> Removed {
        let mut removed = Removed::new();
        for (base, page) in self.pages.remove_range(first, last) {
            if let Some(frame) = page.frame {
                self.tables.uninstall(base);
                removed.touch(base);
                removed.frames.push(frame);
            }
        }
        removed
    }

    fn fail(&mut self, first: u64, last: u64, error: NativeError) -> Failed {
        Failed {
            error,
            removed: self.take(first, last),
        }
    }

    /// Gives discarded page `base` a zero-filled frame.
    fn commit(&mut self, base: u64, flags: PageFlags) -> Result<(), NativeError> {
        let processor = helios_hal::cpu::current_processor();
        let frame =
            allocate_user_frame_zeroed_on(processor).map_err(|_| NativeError::OutOfMemory)?;
        if let Err(error) = self
            .tables
            .install(base, self.tables.physical(frame), flags)
        {
            deallocate_user_frame_on(processor, frame);
            return Err(error);
        }
        self.pages.commit(base, frame);
        Ok(())
    }

    /// Gives every discarded page of `[first, last)` its frame. Pages
    /// committed before a failure keep theirs: they read as zeros either
    /// way.
    fn commit_range(&mut self, first: u64, last: u64) -> Result<(), NativeError> {
        let discarded: Vec<(u64, PageFlags)> = self
            .pages
            .pages
            .range(first..last)
            .filter(|(_, page)| page.frame.is_none())
            .map(|(base, page)| (*base, page.flags))
            .collect();
        discarded
            .into_iter()
            .try_for_each(|(base, flags)| self.commit(base, flags))
    }
}

fn page_span(range: VirtRange) -> Result<(u64, u64), NativeError> {
    if !range.is_page_aligned() || range.byte_len == 0 {
        return Err(NativeError::Address);
    }
    let first = range.start.raw() as u64;
    let last = first
        .checked_add(range.byte_len as u64)
        .ok_or(NativeError::Address)?;
    Ok((first, last))
}

// SAFETY: every user page maps a frame this space allocated, with the flags
// it was mapped with, through a translation that keeps `UserTranslation`'s
// contract; a frame leaves the space only after `complete` has shot down
// every translation of it.
unsafe impl<Tables: UserTranslation> UserSpace for NativeSpace<Tables> {
    fn map(&self, range: VirtRange, contents: &[u8], flags: PageFlags) -> Result<(), NativeError> {
        if contents.len() > range.byte_len {
            return Err(NativeError::Address);
        }
        let start = range.start.raw() as u64;
        let mut state = self.shared.state.lock();
        let (first, last) = state.pages.span(start, range.byte_len as u64)?;
        if state.pages.overlaps(first, last) {
            return Err(NativeError::Overlap);
        }
        if let Err(failed) = state.install(first, last, flags) {
            self.complete(&state.tables, failed.removed);
            return Err(failed.error);
        }
        state
            .pages
            .write(start, contents, PageFlags::empty())
            .expect("installed pages cover the range");
        Ok(())
    }

    fn map_anywhere(
        &self,
        hint: VirtAddr,
        byte_len: usize,
        flags: PageFlags,
    ) -> Result<VirtRange, NativeError> {
        if byte_len == 0 || !(byte_len as u64).is_multiple_of(PAGE) {
            return Err(NativeError::Address);
        }
        let mut state = self.shared.state.lock();
        let start = state
            .pages
            .free_range(hint.raw() as u64, byte_len as u64)
            .ok_or(NativeError::OutOfMemory)?;
        if let Err(failed) = state.install(start, start + byte_len as u64, flags) {
            self.complete(&state.tables, failed.removed);
            return Err(failed.error);
        }
        Ok(VirtRange::new(VirtAddr::new(start as usize), byte_len))
    }

    fn read(&self, address: VirtAddr, output: &mut [u8]) -> Result<(), NativeError> {
        self.shared
            .state
            .lock()
            .pages
            .read(address.raw() as u64, output)
    }

    fn check_writable(&self, address: VirtAddr, byte_len: usize) -> Result<(), NativeError> {
        self.shared
            .state
            .lock()
            .pages
            .check(address.raw() as u64, byte_len, PageFlags::WRITE)
    }

    fn write(&self, address: VirtAddr, bytes: &[u8]) -> Result<(), NativeError> {
        let address = address.raw() as u64;
        let mut state = self.shared.state.lock();
        state.pages.check(address, bytes.len(), PageFlags::WRITE)?;
        if bytes.is_empty() {
            return Ok(());
        }
        let (first, last) = state.pages.span(address, bytes.len() as u64)?;
        state.commit_range(first, last)?;
        state.pages.write(address, bytes, PageFlags::WRITE)
    }

    fn atomic_load_u32(&self, address: VirtAddr) -> Result<u32, NativeError> {
        let address = address.raw() as u64;
        if address & 3 != 0 {
            return Err(NativeError::Address);
        }
        let state = self.shared.state.lock();
        let (first, _) = state.pages.span(address, 4)?;
        let page = state.pages.get(first).ok_or(NativeError::NotMapped)?;
        if !page.flags.contains(PageFlags::READ) {
            return Err(NativeError::NotMapped);
        }
        let Some(frame) = page.frame else {
            return Ok(0);
        };
        let offset = (address - first) as usize;
        // SAFETY: `address` is 4-byte aligned inside `first`'s 4 KiB frame,
        // which stays allocated while `state` is locked.
        let word = unsafe { &*frame.as_ptr().add(offset).cast::<AtomicU32>() };
        Ok(u32::from_le(word.load(Ordering::SeqCst)))
    }

    fn atomic_store_u32(&self, address: VirtAddr, value: u32) -> Result<(), NativeError> {
        let address = address.raw() as u64;
        if address & 3 != 0 {
            return Err(NativeError::Address);
        }
        let mut state = self.shared.state.lock();
        let (first, _) = state.pages.span(address, 4)?;
        let page = state.pages.get(first).ok_or(NativeError::NotMapped)?;
        if !page.flags.contains(PageFlags::WRITE) {
            return Err(NativeError::NotMapped);
        }
        if page.frame.is_none() {
            state.commit(first, page.flags)?;
        }
        let frame = state
            .pages
            .get(first)
            .and_then(|page| page.frame)
            .expect("atomic store commits a discarded page first");
        let offset = (address - first) as usize;
        // SAFETY: as in `atomic_load_u32`.
        let word = unsafe { &*frame.as_ptr().add(offset).cast::<AtomicU32>() };
        word.store(value.to_le(), Ordering::SeqCst);
        Ok(())
    }

    fn atomic_compare_exchange_u32(
        &self,
        address: VirtAddr,
        expected: u32,
        desired: u32,
    ) -> Result<Result<u32, u32>, NativeError> {
        let address = address.raw() as u64;
        if address & 3 != 0 {
            return Err(NativeError::Address);
        }
        let mut state = self.shared.state.lock();
        let (first, _) = state.pages.span(address, 4)?;
        let page = state.pages.get(first).ok_or(NativeError::NotMapped)?;
        if !page.flags.contains(PageFlags::READ | PageFlags::WRITE) {
            return Err(NativeError::NotMapped);
        }
        if page.frame.is_none() {
            state.commit(first, page.flags)?;
        }
        let frame = state
            .pages
            .get(first)
            .and_then(|page| page.frame)
            .expect("atomic compare-exchange commits a discarded page first");
        let offset = (address - first) as usize;
        // SAFETY: as in `atomic_load_u32`.
        let word = unsafe { &*frame.as_ptr().add(offset).cast::<AtomicU32>() };
        Ok(word
            .compare_exchange(
                expected.to_le(),
                desired.to_le(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map(u32::from_le)
            .map_err(u32::from_le))
    }

    fn unmap(&self, range: VirtRange) -> Result<(), NativeError> {
        let (first, last) = page_span(range)?;
        let mut state = self.shared.state.lock();
        state.pages.span(first, last - first)?;
        let removed = state.take(first, last);
        self.complete(&state.tables, removed);
        Ok(())
    }

    fn protect(&self, range: VirtRange, flags: PageFlags) -> Result<(), NativeError> {
        let (first, last) = page_span(range)?;
        let mut state = self.shared.state.lock();
        state.pages.span(first, last - first)?;
        if !state.pages.covers(first, last) {
            return Err(NativeError::NotMapped);
        }
        let SpaceState { pages, tables } = &mut *state;
        let mut narrowed = Removed::new();
        for (base, page) in pages.pages.range_mut(first..last) {
            if page.frame.is_some() && page.flags != flags {
                tables.protect(*base, flags);
                if !flags.contains(page.flags) {
                    narrowed.touch(*base);
                }
            }
            page.flags = flags;
        }
        self.complete(&state.tables, narrowed);
        Ok(())
    }

    fn discard(&self, range: VirtRange) -> Result<(), NativeError> {
        let (first, last) = page_span(range)?;
        let mut state = self.shared.state.lock();
        state.pages.span(first, last - first)?;
        let SpaceState { pages, tables } = &mut *state;
        let mut removed = Removed::new();
        for (base, page) in pages.pages.range_mut(first..last) {
            if let Some(frame) = page.frame.take() {
                tables.uninstall(*base);
                removed.touch(*base);
                removed.frames.push(frame);
            }
        }
        self.complete(&state.tables, removed);
        Ok(())
    }

    fn remap(&self, from: VirtRange, to: VirtRange) -> Result<(), NativeError> {
        let (from_first, from_last) = page_span(from)?;
        let (to_first, to_last) = page_span(to)?;
        if to.byte_len < from.byte_len {
            return Err(NativeError::Address);
        }
        if from_first < to_last && to_first < from_last {
            return Err(NativeError::Overlap);
        }
        let mut state = self.shared.state.lock();
        state.pages.span(from_first, from_last - from_first)?;
        state.pages.span(to_first, to_last - to_first)?;
        if state.pages.overlaps(to_first, to_last) {
            return Err(NativeError::Overlap);
        }
        let flags = state
            .pages
            .uniform(from_first, from_last)
            .ok_or(NativeError::NotMapped)?;
        let moved_last = to_first + (from_last - from_first);
        // The pages past the moved ones first: their frames are the move's
        // only new user memory, taken while nothing has moved yet.
        if let Err(failed) = state.install(moved_last, to_last, flags) {
            self.complete(&state.tables, failed.removed);
            return Err(failed.error);
        }
        // Each moved frame is reachable at both addresses until the source
        // goes, so a slice racing the move reaches one frame, never two.
        let committed: Vec<(u64, NonNull<u8>)> = state
            .pages
            .pages
            .range(from_first..from_last)
            .filter_map(|(base, page)| page.frame.map(|frame| (*base, frame)))
            .collect();
        for (index, (base, frame)) in committed.iter().enumerate() {
            let target = to_first + (base - from_first);
            let physical = state.tables.physical(*frame);
            if let Err(error) = state.tables.install(target, physical, flags) {
                // Unwind the destination. Its moved translations alias
                // frames the source still owns, so only the extension's
                // frames are freed.
                let mut unwound = state.take(moved_last, to_last);
                for (base, _) in &committed[..index] {
                    let target = to_first + (base - from_first);
                    state.tables.uninstall(target);
                    unwound.touch(target);
                }
                self.complete(&state.tables, unwound);
                return Err(error);
            }
        }
        let SpaceState { pages, tables } = &mut *state;
        let mut source = Removed::new();
        for (base, page) in pages.remove_range(from_first, from_last) {
            if page.frame.is_some() {
                tables.uninstall(base);
                source.touch(base);
            }
            pages.insert(to_first + (base - from_first), page);
        }
        self.complete(&state.tables, source);
        Ok(())
    }

    fn resolve_fault(&self, address: VirtAddr, access: PageFlags) -> Result<(), NativeError> {
        let base = address.page_floor().raw() as u64;
        let mut state = self.shared.state.lock();
        let page = state.pages.get(base).ok_or(NativeError::NotMapped)?;
        if !page.flags.contains(access) {
            return Err(NativeError::NotMapped);
        }
        match page.frame {
            // A translation older than the page's current access, left on
            // the faulting processor: walking the tables again succeeds.
            Some(_) => Ok(()),
            None => state.commit(base, page.flags),
        }
    }

    fn mapping(&self, range: VirtRange) -> Option<Mapping> {
        let (first, last) = page_span(range)
            .unwrap_or_else(|_| panic!("a mapping query names a page-aligned range: {range:?}"));
        self.shared.state.lock().pages.first_run(first, last)
    }

    fn free_range(&self, hint: VirtAddr, byte_len: usize) -> Option<VirtAddr> {
        assert!(
            byte_len != 0 && (byte_len as u64).is_multiple_of(PAGE),
            "a free-range query names a nonzero page multiple, not {byte_len:#x} bytes"
        );
        self.shared
            .state
            .lock()
            .pages
            .free_range(hint.raw() as u64, byte_len as u64)
            .map(|start| VirtAddr::new(start as usize))
    }
}

impl<Tables> Drop for NativeSpace<Tables> {
    fn drop(&mut self) {
        // No slice outlives the borrow of this space, and every slice's exit
        // left no translation of it usable on its processor.
        let processor = helios_hal::cpu::current_processor();
        for page in self.shared.state.get_mut().pages.drain() {
            if let Some(frame) = page.frame {
                deallocate_user_frame_on(processor, frame);
            }
        }
    }
}

/// A mapped user page: its frame's direct-map address, `None` once the
/// frame was discarded, and its access.
#[derive(Clone, Copy, Debug)]
pub struct NativePage {
    pub frame: Option<NonNull<u8>>,
    pub flags: PageFlags,
}

/// Every mapped page of one user address space, by page address.
pub struct NativePages {
    pages: BTreeMap<u64, NativePage>,
    /// The lowest mappable address: page zero stays unmapped.
    start: u64,
    /// The end of the user range.
    end: u64,
}

// SAFETY: the index owns the frames it names; they are reached only through
// `&mut self` or under the owner's lock.
unsafe impl Send for NativePages {}
// SAFETY: as above; shared references only read the map.
unsafe impl Sync for NativePages {}

impl NativePages {
    pub fn new(start: u64, end: u64) -> Self {
        Self {
            pages: BTreeMap::new(),
            start,
            end,
        }
    }

    /// The page-aligned span `[address, address + length)` touches, refused
    /// when it is empty, wraps, or leaves the user range.
    pub fn span(&self, address: u64, length: u64) -> Result<(u64, u64), NativeError> {
        let end = address.checked_add(length).ok_or(NativeError::Address)?;
        if length == 0 || address < self.start || end > self.end {
            return Err(NativeError::Address);
        }
        Ok((address & !(PAGE - 1), end.next_multiple_of(PAGE)))
    }

    pub fn overlaps(&self, first: u64, last: u64) -> bool {
        self.pages.range(first..last).next().is_some()
    }

    /// Whether every page of the page span `[first, last)` is mapped.
    pub fn covers(&self, first: u64, last: u64) -> bool {
        self.pages.range(first..last).count() as u64 == (last - first) / PAGE
    }

    /// The access every page of `[first, last)` is mapped with, `None` when
    /// a page is not mapped or two differ.
    pub fn uniform(&self, first: u64, last: u64) -> Option<PageFlags> {
        if !self.covers(first, last) {
            return None;
        }
        let mut pages = self.pages.range(first..last).map(|(_, page)| page.flags);
        let flags = pages.next()?;
        pages.all(|other| other == flags).then_some(flags)
    }

    /// The first run of `[first, last)` mapped contiguously with one access.
    pub fn first_run(&self, first: u64, last: u64) -> Option<Mapping> {
        let mut pages = self.pages.range(first..last);
        let (&start, head) = pages.next()?;
        let mut end = start + PAGE;
        for (&base, page) in pages {
            if base != end || page.flags != head.flags {
                break;
            }
            end += PAGE;
        }
        Some(Mapping {
            range: VirtRange::new(VirtAddr::new(start as usize), (end - start) as usize),
            flags: head.flags,
        })
    }

    pub fn get(&self, base: u64) -> Option<NativePage> {
        self.pages.get(&base).copied()
    }

    pub fn insert(&mut self, base: u64, page: NativePage) {
        let previous = self.pages.insert(base, page);
        assert!(previous.is_none(), "user page {base:#x} mapped twice");
    }

    /// Gives discarded page `base` the frame `frame`.
    pub fn commit(&mut self, base: u64, frame: NonNull<u8>) {
        let page = self
            .pages
            .get_mut(&base)
            .unwrap_or_else(|| panic!("committing unmapped user page {base:#x}"));
        assert!(
            page.frame.replace(frame).is_none(),
            "user page {base:#x} committed twice"
        );
    }

    /// Takes every page of `[first, last)` out of the index.
    pub fn remove_range(&mut self, first: u64, last: u64) -> Vec<(u64, NativePage)> {
        let mut tail = self.pages.split_off(&first);
        let mut rest = tail.split_off(&last);
        self.pages.append(&mut rest);
        tail.into_iter().collect()
    }

    /// Takes every page out of the index.
    pub fn drain(&mut self) -> impl Iterator<Item = NativePage> {
        core::mem::take(&mut self.pages).into_values()
    }

    /// The first free page-aligned range of `length` bytes at or above
    /// `hint`, else at or above the start of the user range.
    pub fn free_range(&self, hint: u64, length: u64) -> Option<u64> {
        self.find(hint, length)
            .or_else(|| self.find(self.start, length))
    }

    fn find(&self, from: u64, length: u64) -> Option<u64> {
        let mut candidate = (from & !(PAGE - 1)).max(self.start);
        loop {
            let end = candidate
                .checked_add(length)
                .filter(|end| *end <= self.end)?;
            match self.pages.range(candidate..end).next_back() {
                Some((base, _)) => candidate = base + PAGE,
                None => return Some(candidate),
            }
        }
    }

    /// Whether every page `[address, address + length)` touches is mapped
    /// with `access`.
    pub fn check(&self, address: u64, length: usize, access: PageFlags) -> Result<(), NativeError> {
        if length == 0 {
            return Ok(());
        }
        let (first, last) = self.span(address, length as u64)?;
        let mut expected = first;
        for (base, page) in self.pages.range(first..last) {
            if *base != expected || !page.flags.contains(access) {
                return Err(NativeError::NotMapped);
            }
            expected += PAGE;
        }
        if expected != last {
            return Err(NativeError::NotMapped);
        }
        Ok(())
    }

    /// Visits `[address, address + length)` as runs of physically adjacent
    /// frames, after checking that every page is mapped with `access`:
    /// `visit(run, bytes, offset)` receives the run's bytes through the
    /// direct map, or `None` for a run of discarded pages, and the run's
    /// offset within the range. Nothing is visited when any page fails the
    /// check.
    pub fn runs(
        &self,
        address: u64,
        length: usize,
        access: PageFlags,
        mut visit: impl FnMut(Option<NonNull<u8>>, usize, usize),
    ) -> Result<(), NativeError> {
        self.check(address, length, access)?;
        if length == 0 {
            return Ok(());
        }
        let (first, last) = self.span(address, length as u64)?;
        let mut done = 0usize;
        let mut run: Option<(Option<NonNull<u8>>, usize)> = None;
        for (base, page) in self.pages.range(first..last) {
            let skip = address.saturating_sub(*base) as usize;
            let take = (PAGE as usize - skip).min(length - done - run.map_or(0, |(_, n)| n));
            // SAFETY: `skip` lies inside this page's frame.
            let start = page
                .frame
                .map(|frame| unsafe { NonNull::new_unchecked(frame.as_ptr().add(skip)) });
            run = match (run, start) {
                (Some((Some(head), bytes)), Some(start))
                    if head.as_ptr().wrapping_add(bytes) == start.as_ptr() =>
                {
                    Some((Some(head), bytes + take))
                }
                (Some((None, bytes)), None) => Some((None, bytes + take)),
                (Some((head, bytes)), start) => {
                    visit(head, bytes, done);
                    done += bytes;
                    Some((start, take))
                }
                (None, start) => Some((start, take)),
            };
        }
        if let Some((head, bytes)) = run {
            visit(head, bytes, done);
        }
        Ok(())
    }

    /// Copies `output.len()` readable bytes from `address`; a discarded page
    /// reads as zeros.
    pub fn read(&self, address: u64, output: &mut [u8]) -> Result<(), NativeError> {
        let length = output.len();
        self.runs(address, length, PageFlags::READ, |run, bytes, offset| {
            let target = &mut output[offset..offset + bytes];
            match run {
                // SAFETY: the run lies inside frames this index owns, which
                // stay allocated while the owner's lock is held.
                Some(run) => target
                    .copy_from_slice(unsafe { core::slice::from_raw_parts(run.as_ptr(), bytes) }),
                None => target.fill(0),
            }
        })
    }

    /// Copies `bytes` to `address`, every page of which must be writable
    /// and committed; `access` is the access the copy requires, empty for
    /// the initial contents a fresh mapping receives.
    pub fn write(&self, address: u64, bytes: &[u8], access: PageFlags) -> Result<(), NativeError> {
        self.runs(address, bytes.len(), access, |run, length, offset| {
            let run = run.expect("a write commits every discarded page first");
            // SAFETY: as in `read`.
            let target = unsafe { core::slice::from_raw_parts_mut(run.as_ptr(), length) };
            target.copy_from_slice(&bytes[offset..offset + length]);
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use alloc::vec;

    #[repr(C, align(4096))]
    struct Frames([u8; 4 * PAGE as usize]);

    fn frame(frames: &mut Frames, index: usize) -> NonNull<u8> {
        NonNull::new(frames.0[index * PAGE as usize..].as_mut_ptr()).unwrap()
    }

    fn page(frame: NonNull<u8>, flags: PageFlags) -> NativePage {
        NativePage {
            frame: Some(frame),
            flags,
        }
    }

    #[test]
    fn copies_cross_pages_in_one_run_per_adjacent_frame_group() {
        let mut frames = Box::new(Frames([0; 4 * PAGE as usize]));
        let mut pages = NativePages::new(PAGE, 1 << 47);
        let rw = PageFlags::READ | PageFlags::WRITE;
        // Pages 0x10000 and 0x11000 are frames 0 and 1, adjacent; 0x12000
        // is frame 3, apart from them.
        pages.insert(0x10000, page(frame(&mut frames, 0), rw));
        pages.insert(0x11000, page(frame(&mut frames, 1), rw));
        pages.insert(0x12000, page(frame(&mut frames, 3), rw));
        // 27 bytes a repeat, so 500 cover the three pages.
        let text = "中文 日本語 한국어 ".repeat(500).into_bytes();
        let start = 0x10000 + 100;
        let length = 3 * PAGE as usize - 200;
        let mut runs = Vec::new();
        pages
            .runs(start, length, PageFlags::WRITE, |_, bytes, offset| {
                runs.push((offset, bytes))
            })
            .unwrap();
        assert_eq!(
            runs,
            vec![
                (0, 2 * PAGE as usize - 100),
                (2 * PAGE as usize - 100, PAGE as usize - 100)
            ]
        );
        pages
            .write(start, &text[..length], PageFlags::WRITE)
            .unwrap();
        let mut back = vec![0; length];
        pages.read(start, &mut back).unwrap();
        assert_eq!(back, &text[..length]);
        assert_eq!(
            &frames.0[3 * PAGE as usize..3 * PAGE as usize + 10],
            &text[2 * PAGE as usize - 100..2 * PAGE as usize - 90]
        );
    }

    #[test]
    fn a_gap_or_a_missing_access_refuses_the_whole_copy() {
        let mut frames = Box::new(Frames([0; 4 * PAGE as usize]));
        let mut pages = NativePages::new(PAGE, 1 << 47);
        pages.insert(
            0x10000,
            page(frame(&mut frames, 0), PageFlags::READ | PageFlags::WRITE),
        );
        pages.insert(
            0x12000,
            page(frame(&mut frames, 1), PageFlags::READ | PageFlags::WRITE),
        );
        pages.insert(0x13000, page(frame(&mut frames, 2), PageFlags::READ));
        assert_eq!(
            pages.write(0x10ff0, &[1; 32], PageFlags::WRITE),
            Err(NativeError::NotMapped)
        );
        assert_eq!(
            pages.write(0x12ff0, &[1; 32], PageFlags::WRITE),
            Err(NativeError::NotMapped)
        );
        assert!(frames.0.iter().all(|byte| *byte == 0));
        assert_eq!(pages.read(0, &mut [0; 1]), Err(NativeError::Address));
        assert_eq!(pages.read(u64::MAX, &mut [0; 2]), Err(NativeError::Address));
    }

    #[test]
    fn free_ranges_skip_mapped_pages_and_fall_back_below_the_hint() {
        let mut frames = Box::new(Frames([0; 4 * PAGE as usize]));
        let mut pages = NativePages::new(PAGE, 0x20000);
        pages.insert(0x10000, page(frame(&mut frames, 0), PageFlags::READ));
        assert_eq!(pages.free_range(0x10000, 2 * PAGE), Some(0x11000));
        assert_eq!(pages.free_range(0x1f000, 2 * PAGE), Some(PAGE));
        assert_eq!(pages.free_range(0, 0x20000), None);
    }

    #[test]
    fn discarded_pages_read_as_zeros_between_committed_runs() {
        let mut frames = Box::new(Frames([0; 4 * PAGE as usize]));
        frames.0.fill(0x5a);
        let mut pages = NativePages::new(PAGE, 1 << 47);
        let rw = PageFlags::READ | PageFlags::WRITE;
        pages.insert(0x10000, page(frame(&mut frames, 0), rw));
        pages.insert(
            0x11000,
            NativePage {
                frame: None,
                flags: rw,
            },
        );
        pages.insert(0x12000, page(frame(&mut frames, 2), rw));
        let mut runs = Vec::new();
        pages
            .runs(
                0x10000,
                3 * PAGE as usize,
                PageFlags::READ,
                |run, bytes, offset| runs.push((run.is_some(), offset, bytes)),
            )
            .unwrap();
        let page = PAGE as usize;
        assert_eq!(
            runs,
            vec![(true, 0, page), (false, page, page), (true, 2 * page, page)]
        );
        let mut back = vec![1; 3 * page];
        pages.read(0x10000, &mut back).unwrap();
        assert!(back[..page].iter().all(|byte| *byte == 0x5a));
        assert!(back[page..2 * page].iter().all(|byte| *byte == 0));
        assert!(back[2 * page..].iter().all(|byte| *byte == 0x5a));
    }

    #[test]
    fn runs_report_the_first_uniform_stretch_and_ranges_split_cleanly() {
        let mut frames = Box::new(Frames([0; 4 * PAGE as usize]));
        let mut pages = NativePages::new(PAGE, 1 << 47);
        let rw = PageFlags::READ | PageFlags::WRITE;
        pages.insert(0x10000, page(frame(&mut frames, 0), rw));
        pages.insert(0x11000, page(frame(&mut frames, 1), rw));
        pages.insert(0x12000, page(frame(&mut frames, 2), PageFlags::READ));
        pages.insert(0x14000, page(frame(&mut frames, 3), PageFlags::READ));
        let run = |first, last| {
            pages
                .first_run(first, last)
                .map(|run| (run.range, run.flags))
        };
        let range = |start: usize, count: usize| {
            VirtRange::new(VirtAddr::new(start), count * PAGE as usize)
        };
        assert_eq!(run(0xf000, 0x15000), Some((range(0x10000, 2), rw)));
        assert_eq!(run(0x11000, 0x15000), Some((range(0x11000, 1), rw)));
        assert_eq!(
            run(0x12000, 0x15000),
            Some((range(0x12000, 1), PageFlags::READ))
        );
        assert_eq!(run(0x13000, 0x14000), None);
        assert_eq!(pages.uniform(0x10000, 0x12000), Some(rw));
        assert_eq!(pages.uniform(0x10000, 0x13000), None);
        assert_eq!(pages.uniform(0x12000, 0x15000), None);
        assert!(pages.covers(0x10000, 0x13000) && !pages.covers(0x10000, 0x14000));
        let taken: Vec<u64> = pages
            .remove_range(0x11000, 0x14000)
            .into_iter()
            .map(|(base, _)| base)
            .collect();
        assert_eq!(taken, vec![0x11000, 0x12000]);
        assert!(pages.get(0x10000).is_some() && pages.get(0x14000).is_some());
        assert!(!pages.overlaps(0x11000, 0x14000));
    }

    struct ShootdownLog {
        events: Mutex<Vec<(u64, u64, usize)>>,
    }

    // SAFETY: test model records invalidations and checks that removed frames
    // are still allocated when `invalidate` runs.
    unsafe impl UserTranslation for alloc::sync::Arc<ShootdownLog> {
        fn install(&mut self, _: u64, _: u64, _: PageFlags) -> Result<(), NativeError> {
            Ok(())
        }

        fn uninstall(&mut self, _: u64) {}

        fn protect(&mut self, _: u64, _: PageFlags) {}

        fn invalidate(&self, first: u64, last: u64) {
            self.events.lock().push((
                first,
                last,
                crate::memory::user_heap_stats().allocated_bytes,
            ));
        }

        fn physical(&self, frame: NonNull<u8>) -> u64 {
            frame.as_ptr() as u64
        }
    }

    fn ensure_pool() {
        const POOL_BYTES: usize = 4 * 1024 * 1024;
        if crate::memory::installed_user_memory_pool().is_some() {
            return;
        }
        let layout =
            alloc::alloc::Layout::from_size_align(POOL_BYTES, PAGE as usize).expect("pool layout");
        // SAFETY: nonzero size, leaked for the process lifetime of the test pool.
        let base = unsafe { alloc::alloc::alloc_zeroed(layout) } as usize;
        assert!(base != 0, "pool backing");
        crate::memory::install_user_memory_pool(crate::memory::UserMemoryPool::single_node(
            1,
            &[(base, base + POOL_BYTES)],
        ));
    }

    #[test]
    fn shootdown_runs_when_slices_are_active_and_before_frames_reclaim() {
        ensure_pool();
        let log = alloc::sync::Arc::new(ShootdownLog {
            events: Mutex::new(Vec::new()),
        });
        let space = NativeSpace::new(log.clone(), PAGE, 1 << 47);
        let rw = PageFlags::READ | PageFlags::WRITE;
        let range = |start: usize, pages: usize| {
            VirtRange::new(VirtAddr::new(start), pages * PAGE as usize)
        };

        // Without a running slice, removals skip remote invalidation.
        space.map(range(0x10000, 1), b"solo", rw).unwrap();
        space.unmap(range(0x10000, 1)).unwrap();
        assert!(log.events.lock().is_empty());

        // With a sibling slice running, map and widening protect need no
        // shootdown, while narrowing protect, discard, remap and unmap shoot
        // down before freeing removed frames.
        let slice = space.begin_slice();
        space
            .map(range(0x20000, 2), b"shared", PageFlags::READ)
            .unwrap();
        space.protect(range(0x20000, 2), rw).unwrap();
        assert!(log.events.lock().is_empty());

        space.protect(range(0x20000, 1), PageFlags::READ).unwrap();
        assert_eq!(log.events.lock().len(), 1);
        assert_eq!(log.events.lock()[0].0, 0x20000);
        assert_eq!(log.events.lock()[0].1, 0x21000);

        let before_discard = crate::memory::user_heap_stats().allocated_bytes;
        space.discard(range(0x21000, 1)).unwrap();
        let after_discard = crate::memory::user_heap_stats().allocated_bytes;
        let (_, _, during_discard) = *log.events.lock().last().unwrap();
        assert_eq!(during_discard, before_discard);
        assert!(after_discard < before_discard);

        // Atomic 32-bit operations read zeros from discarded pages, commit on
        // write/CAS, and enforce alignment and access flags.
        assert_eq!(space.atomic_load_u32(VirtAddr::new(0x21000)), Ok(0));
        assert_eq!(
            space.atomic_load_u32(VirtAddr::new(0x21002)),
            Err(NativeError::Address)
        );
        assert_eq!(
            space.atomic_store_u32(VirtAddr::new(0x20000), 9),
            Err(NativeError::NotMapped)
        );
        assert_eq!(
            space.atomic_compare_exchange_u32(VirtAddr::new(0x21004), 0, 0x1234_5678),
            Ok(Ok(0))
        );
        assert_eq!(
            space.atomic_load_u32(VirtAddr::new(0x21004)),
            Ok(0x1234_5678)
        );
        assert_eq!(
            space.atomic_compare_exchange_u32(VirtAddr::new(0x21004), 0, 7),
            Ok(Err(0x1234_5678))
        );

        space.protect(range(0x20000, 2), rw).unwrap();
        space.remap(range(0x20000, 2), range(0x30000, 2)).unwrap();
        assert_eq!(log.events.lock().last().unwrap().0, 0x20000);
        assert_eq!(log.events.lock().last().unwrap().1, 0x22000);

        let before_unmap = crate::memory::user_heap_stats().allocated_bytes;
        space.unmap(range(0x30000, 2)).unwrap();
        let after_unmap = crate::memory::user_heap_stats().allocated_bytes;
        let (first, last, during_unmap) = *log.events.lock().last().unwrap();
        assert_eq!((first, last), (0x30000, 0x32000));
        assert_eq!(during_unmap, before_unmap);
        assert!(after_unmap < before_unmap);

        drop(slice);
    }
}
