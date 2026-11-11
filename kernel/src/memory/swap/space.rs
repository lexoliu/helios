//! The address-space half of swap, written once for every backend.
//!
//! A backend's address space forwards its `swap_out_page`, `swap_in_page`,
//! `scan_committed_pages` and `drain_orphaned_swap_tokens` here and
//! supplies the page-table encoding through [`SwapLeafFormat`].
//!
//! # Concurrency contract
//!
//! Every method that takes a `tracker` runs with the address space's
//! reservation lock held by the caller: the lock is what serialises every
//! software write to a user leaf, and the tracker is the proof of it. The
//! lock is a task-context spin lock that no interrupt handler or trap
//! path takes, so a processor waiting for it keeps acknowledging TLB
//! shootdowns. Inside it [`SwapSpace`] takes the user pool's IRQ-masking
//! lock for a frame and its own `orphans` lock, and the order is always
//! reservation lock first: reservation lock then `orphans`, reservation
//! lock then the pool.
//!
//! [`SwapSpace::drain_orphaned_swap_tokens`] takes only `orphans`. Nothing here
//! blocks, awaits or fans out to another processor beyond what a
//! [`SwapLeafFormat::detach`] does for the shootdown it owes.
//!
//! # Order of a swap-out
//!
//! The page is read after it is gone. The leaf is replaced and every
//! processor has dropped its translation before a single byte is copied,
//! so a store from a processor that was running the instance lands in the
//! frame before the copy or faults on the swap entry: it cannot land in a
//! frame whose contents were already taken. Copying first, as the
//! backends used to, loses such a store silently.

extern crate alloc;

use alloc::vec::Vec;
use core::ptr;

use helios_hal::cpu::current_processor;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::{AddressSpaceError, PageAge, PageFlags, SwapToken, VirtAddr};

use super::leaf::{AGE_BATCH_PAGES, SwapLeafFormat};
use crate::memory::owner::MemoryOwner;
use crate::memory::user::{allocate_user_frame_uninit_on, deallocate_user_frame_on};
use crate::memory::{ReservationTracker, SwapEntry};

/// What every backend's swap does that is not a page-table encoding.
#[derive(Default)]
pub struct SwapSpace {
    /// Tokens whose pages went away underneath them, waiting for the swap
    /// task to hand them back to the backend.
    ///
    /// They cannot be released where they are found: that runs under the
    /// reservation lock, and releasing an extent is asynchronous.
    orphans: spin::Mutex<Vec<SwapToken>>,
}

impl SwapSpace {
    pub const fn new() -> Self {
        Self {
            orphans: spin::Mutex::new(Vec::new()),
        }
    }

    /// Detaches the committed page at `addr` and copies it into `out`.
    ///
    /// The tracker records the page as swapped, the format replaces the
    /// leaf and completes the shootdown, and only then are the bytes read
    /// from the frame, which is freed last. A failed detach leaves the
    /// tracker as it was and frees nothing.
    pub fn swap_out_page<F: SwapLeafFormat>(
        &self,
        tracker: &mut ReservationTracker,
        format: &F,
        addr: VirtAddr,
        token: SwapToken,
        out: &mut [u8],
    ) -> Result<PageFlags, AddressSpaceError> {
        if out.len() != PhysFrame::SIZE {
            return Err(AddressSpaceError::BadPageBuffer);
        }
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let flags = tracker.record_swap_out(addr, token)?;
        let frame = match format.detach(tracker, addr, token) {
            Ok(frame) => frame,
            Err(error) => {
                // Nothing moved; put the bookkeeping back the way it was.
                restore_committed(tracker, addr);
                return Err(error);
            }
        };
        // SAFETY: `frame` is one whole frame this address space allocated,
        // and `detach` returned only after every processor dropped its
        // translation, so nothing writes it any more. `out` is exactly one
        // page long (checked above) and cannot overlap a pool frame.
        unsafe {
            ptr::copy_nonoverlapping(frame.as_ptr(), out.as_mut_ptr(), PhysFrame::SIZE);
        }
        deallocate_user_frame_on(current_processor(), frame);
        Ok(flags)
    }

    /// Puts a swapped-out page back from `bytes`: fills a fresh frame
    /// first and publishes the mapping second, so no processor sees a
    /// half-restored page.
    ///
    /// Returns the token the page carried. A refused reinstatement leaves
    /// the page swapped out, in the tracker and in the leaf, and returns
    /// the frame.
    ///
    /// # Panics
    ///
    /// If the tracker holds the page swapped out and the format says its
    /// leaf does not carry that token: the two are written under one
    /// lock, so they cannot disagree.
    pub fn swap_in_page<F: SwapLeafFormat>(
        &self,
        tracker: &mut ReservationTracker,
        format: &F,
        addr: VirtAddr,
        bytes: &[u8],
    ) -> Result<SwapToken, AddressSpaceError> {
        if bytes.len() != PhysFrame::SIZE {
            return Err(AddressSpaceError::BadPageBuffer);
        }
        if !addr.is_page_aligned() {
            return Err(AddressSpaceError::Misaligned);
        }
        let entry = tracker
            .swap_entry(addr)
            .ok_or(AddressSpaceError::NotSwapped)?;
        // Not zeroed: the whole frame is overwritten below.
        let frame = allocate_user_frame_uninit_on(current_processor())
            .map_err(|_| AddressSpaceError::OutOfFrames)?;
        // SAFETY: `frame` is one whole frame this call owns exclusively,
        // `bytes` is exactly one page (checked above), and they cannot
        // overlap.
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), frame.as_ptr(), PhysFrame::SIZE);
        }
        // The page counts as committed from here, which is what a format
        // that derives hardware state from the bookkeeping needs to see.
        let taken = tracker
            .take_swap_entry(addr)
            .expect("a swap entry the tracker just returned is still there under the same lock");
        debug_assert_eq!(taken, entry);
        match format.attach(tracker, addr, entry.token, frame, entry.flags) {
            Ok(()) => Ok(entry.token),
            Err(AddressSpaceError::NotSwapped) => panic!(
                "swap bookkeeping and the page table disagree at {:#x}: the tracker holds a swap \
                 entry for token {}, the leaf carries {:?}",
                addr.raw(),
                entry.token.raw(),
                format.swapped_token(addr).map(SwapToken::raw),
            ),
            Err(error) => {
                deallocate_user_frame_on(current_processor(), frame);
                restore_swapped(tracker, addr, entry);
                Err(error)
            }
        }
    }

    /// Reports how recently the hardware saw each of `owner`'s committed
    /// pages and clears the accessed flag behind the scan.
    ///
    /// `visit` returns `false` to stop. The return value is the number of
    /// present pages visited; a committed page whose leaf is not present
    /// (lazy commit that nothing has touched yet) is skipped, not
    /// counted.
    ///
    /// Cleared flags are reported to the format in runs of consecutive
    /// pages, at most [`AGE_BATCH_PAGES`] each, so a format that has to
    /// invalidate translations covers exactly the pages it cleared.
    pub fn scan_committed_pages<F, Visit>(
        &self,
        tracker: &ReservationTracker,
        format: &F,
        owner: u64,
        mut visit: Visit,
    ) -> usize
    where
        F: SwapLeafFormat,
        Visit: FnMut(VirtAddr, PageFlags, PageAge) -> bool,
    {
        let mut visited = 0_usize;
        tracker.owned_committed_regions(MemoryOwner::new(owner), |region| {
            let mut keep_going = true;
            let mut run_start = region.range.start;
            let mut run_pages = 0_usize;
            for offset in (0..region.range.byte_len).step_by(PhysFrame::SIZE) {
                let addr = VirtAddr::new(region.range.start.raw() + offset);
                let Some(age) = format.test_and_clear_accessed(addr) else {
                    continue;
                };
                visited += 1;
                if age == PageAge::Hot {
                    let contiguous = run_pages != 0
                        && addr.raw() == run_start.raw() + run_pages * PhysFrame::SIZE;
                    if run_pages != 0 && !contiguous {
                        format.accessed_cleared(run_start, run_pages);
                        run_pages = 0;
                    }
                    if run_pages == 0 {
                        run_start = addr;
                    }
                    run_pages += 1;
                    if run_pages == AGE_BATCH_PAGES {
                        format.accessed_cleared(run_start, run_pages);
                        run_pages = 0;
                    }
                }
                if !visit(addr, region.flags, age) {
                    keep_going = false;
                    break;
                }
            }
            if run_pages != 0 {
                format.accessed_cleared(run_start, run_pages);
            }
            keep_going
        });
        visited
    }

    /// Drops swap entries the tracker handed back from a release, a
    /// decommit, a commit or a substitution over them: clears each leaf
    /// that still carries a swap entry and queues its token for the swap
    /// task.
    ///
    /// Only the entries it is given are touched: a page of the range that
    /// was never swapped out has its own leaf and is not this method's.
    pub fn discard<F: SwapLeafFormat>(&self, format: &F, entries: Vec<SwapEntry>) {
        if entries.is_empty() {
            return;
        }
        for entry in &entries {
            format.clear_swapped(entry.addr);
        }
        self.orphans
            .lock()
            .extend(entries.into_iter().map(|entry| entry.token));
    }

    /// Hands each token [`Self::discard`] queued to `visit`, once, and
    /// returns how many there were.
    pub fn drain_orphaned_swap_tokens<Visit>(&self, mut visit: Visit) -> usize
    where
        Visit: FnMut(SwapToken),
    {
        let drained: Vec<SwapToken> = core::mem::take(&mut *self.orphans.lock());
        let count = drained.len();
        for token in drained {
            visit(token);
        }
        count
    }
}

/// Undoes `record_swap_out` for a page whose detach failed.
fn restore_committed(tracker: &mut ReservationTracker, addr: VirtAddr) {
    tracker
        .take_swap_entry(addr)
        .expect("a swap entry recorded under this lock is still there");
}

/// Undoes `take_swap_entry` for a page whose reinstatement failed.
fn restore_swapped(tracker: &mut ReservationTracker, addr: VirtAddr, entry: SwapEntry) {
    let flags = tracker
        .record_swap_out(addr, entry.token)
        .expect("a page committed under this lock is still committed");
    debug_assert_eq!(flags, entry.flags);
}
