//! Multi-producer, single-consumer return stacks.
//!
//! A faulting processor gives back a slot or a store frame it has just
//! emptied, and the owner task takes them back. Both are Treiber stacks
//! whose links live in the structure being stacked (a slot word, a store
//! frame's `dirty_next`), so the stack itself allocates nothing.
//!
//! # Concurrency contract
//!
//! - [`ReturnStack::push`] runs on any processor, in task context. It is
//!   lock-free: it writes the new top's link, then publishes it with one
//!   compare-and-swap on the head, retrying only against another push.
//!   It never blocks, awaits or allocates.
//! - [`ReturnStack::take_all`] runs on the owner task only. It empties the
//!   stack with one `swap`, so no pop ever races a push on a stale head
//!   and the ABA case of a Treiber pop does not exist. The returned
//!   [`Chain`] belongs to the owner alone and is walked without further
//!   synchronisation: every link in it was published before the `swap`
//!   observed it.
//! - A handle is pushed by whoever holds exclusive use of it, and is not
//!   touched by anyone else until the owner takes it.
//!
//! The head is on a cache line of its own because every processor that
//! returns something writes it; the processors that push write the link of
//! the handle they hold, which the structure owning the links places.

use core::num::NonZeroU32;
use core::sync::atomic::{AtomicU32, Ordering};

use crossbeam_utils::CachePadded;

/// Where the links of a stack's handles are kept.
pub(super) trait Links {
    /// The handle stacked under `handle`, or `None` at the bottom.
    fn next_of(&self, handle: NonZeroU32) -> Option<NonZeroU32>;
    /// Points `handle` at the handle stacked under it. Called only by the
    /// processor that holds `handle`, before it publishes it.
    fn set_next(&self, handle: NonZeroU32, next: Option<NonZeroU32>);
}

/// An MPSC stack of non-zero handles. Zero is the empty head.
pub(super) struct ReturnStack {
    /// Written by every processor that returns a handle.
    head: CachePadded<AtomicU32>,
}

impl ReturnStack {
    pub(super) const fn new() -> Self {
        Self {
            head: CachePadded::new(AtomicU32::new(0)),
        }
    }

    /// Returns `handle` to the owner. Any processor.
    pub(super) fn push<L: Links>(&self, links: &L, handle: NonZeroU32) {
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            links.set_next(handle, NonZeroU32::new(head));
            // Release publishes the link just written and everything the
            // pusher did to the handle's contents before giving it up.
            match self.head.compare_exchange_weak(
                head,
                handle.get(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }

    /// Takes every handle pushed so far, most recent first. The owner only.
    pub(super) fn take_all(&self) -> Chain {
        Chain(NonZeroU32::new(self.head.swap(0, Ordering::Acquire)))
    }
}

/// Handles taken from a [`ReturnStack`], owned by the task that took them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Chain(Option<NonZeroU32>);

impl Chain {
    pub(super) const fn empty() -> Self {
        Self(None)
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    pub(super) fn pop<L: Links>(&mut self, links: &L) -> Option<NonZeroU32> {
        let top = self.0?;
        self.0 = links.next_of(top);
        Some(top)
    }
}
