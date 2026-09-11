//! A fixed-capacity set of futures one task drives at once.
//!
//! `FuturesUnordered` is the same surface built on the heap: every
//! future pushed to it is a task node allocated for the occasion, which
//! on the audio path is one allocation per period the device plays.
//! `InFlight` keeps `N` futures in an inline array instead — a future
//! lands in a slot on `push`, is polled through a pin projection of
//! that slot, and is dropped in place when it completes — so driving
//! the set allocates nothing, ever.
//!
//! The futures are structurally pinned. A slot that holds one is a
//! stable address for as long as the set is, and nothing ever moves a
//! `Some` slot: `push` writes only `None` slots, `poll_next` reaches
//! the `Some` ones through `Pin<&mut Option<F>>::as_pin_mut`, and a
//! completed future is cleared by `Pin::set`, which drops it where it
//! sits. What the `unsafe` projections rely on is the set itself being
//! pinned before any future goes in, which is why `poll_next` takes
//! `Pin<&mut Self>` and a `pin!` is how callers hold it.
//!
//! # Concurrency contract
//!
//! Owned by the one task that polls it and never shared. Every slot's
//! future is driven through that task's `Context`, so a `wake` from any
//! of them is a wake on that task, and the next `poll_next` finds the
//! completion — there is no ready queue, only the slots.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

/// Up to `N` futures, polled as a stream of their completions.
///
/// A completion is reported as soon as a poll finds it, in whichever
/// slot order it sits — the set makes no promise about which of two
/// ready futures answers first, only that both are seen.
pub struct InFlight<F, const N: usize> {
    slots: [Option<F>; N],
    len: usize,
}

impl<F, const N: usize> InFlight<F, N> {
    /// An empty set.
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
            len: 0,
        }
    }

    /// How many futures the set is driving right now.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the set is driving nothing.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Add `future` to the set.
    ///
    /// Takes the pinned set rather than `&mut self` so that the contract
    /// a slot's address never changes holds even once futures have been
    /// pushed and polled.
    ///
    /// # Panics
    ///
    /// Panics when the set is full. `N` is the capacity its caller
    /// chose; pushing past it is that caller's bookkeeping bug.
    pub fn push(self: Pin<&mut Self>, future: F) {
        // SAFETY: pushing writes a slot that is `None` and touches
        // nothing that is `Some`, so no pinned future's address changes.
        let this = unsafe { self.get_unchecked_mut() };
        let slot = this
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("an InFlight is pushed only while a slot is free");
        // SAFETY: `slot` is a stable address inside the pinned set, so
        // the future it now holds is pinned there for its whole life.
        unsafe { Pin::new_unchecked(slot) }.set(Some(future));
        this.len += 1;
    }
}

impl<F: Future, const N: usize> InFlight<F, N> {
    /// Poll every slot once, reporting the first completion found.
    ///
    /// `Ready(Some)` is a finished future's output; its slot is free for
    /// the next `push`. `Ready(None)` — the stream convention for a set
    /// that has ended — is what an empty set reports. `Pending` means
    /// something is outstanding and nothing finished this pass; the
    /// caller's waker is the one every slot registered with, so the next
    /// completion wakes this task.
    pub fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<F::Output>> {
        // SAFETY: the slots live in the pinned set at stable addresses,
        // and a `Some` slot's future is reached only through this
        // projection — nothing else moves it.
        let this = unsafe { self.get_unchecked_mut() };
        for slot in this.slots.iter_mut() {
            // SAFETY: `slot` is a stable address in the pinned set, and
            // clearing a completed future through `Pin::set` drops it in
            // place rather than moving it out.
            let mut slot = unsafe { Pin::new_unchecked(slot) };
            let Some(future) = slot.as_mut().as_pin_mut() else {
                continue;
            };
            if let Poll::Ready(output) = future.poll(cx) {
                slot.set(None);
                this.len -= 1;
                return Poll::Ready(Some(output));
            }
        }
        if this.len == 0 {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }
}

impl<F: Future, const N: usize> futures::stream::Stream for InFlight<F, N> {
    type Item = F::Output;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        InFlight::poll_next(self, cx)
    }
}

impl<F, const N: usize> Default for InFlight<F, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use core::future::{Future, pending, ready};
    use core::pin::{Pin, pin};
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use core::task::{Context, Poll};

    use futures_lite::future::{block_on, poll_once};

    use super::InFlight;

    /// A future that counts its polls and completes on the `n`th.
    struct Countdown {
        polls: Arc<AtomicUsize>,
        ready_on: usize,
        output: u32,
    }

    impl Future for Countdown {
        type Output = u32;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            if self.polls.fetch_add(1, Ordering::AcqRel) + 1 < self.ready_on {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Ready(self.output)
        }
    }

    /// A future that never completes but reports when it is dropped.
    struct Dropped(Arc<AtomicBool>);

    impl Future for Dropped {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }

    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// One `poll_next`, awaited — tests read the set the way a pump
    /// does: a `Some` is a completion, a `None` is the end.
    fn next_of<F: Future, const N: usize>(
        mut set: Pin<&mut InFlight<F, N>>,
    ) -> impl Future<Output = Option<F::Output>> {
        core::future::poll_fn(move |cx| set.as_mut().poll_next(cx))
    }

    /// An empty set reports its end rather than parking: the same
    /// convention `FuturesUnordered` has.
    #[test]
    fn an_empty_set_reports_its_end() {
        block_on(async {
            let set = pin!(InFlight::<Countdown, 4>::new());
            assert_eq!(next_of(set).await, None);
        });
    }

    /// Futures are driven through the task's own context: one that is
    /// pending stays pending, and the set ends only once every slot is
    /// done, not when the first poll finds nothing ready.
    #[test]
    fn a_completion_comes_back_with_its_slot_freed() {
        block_on(async {
            let polls = Arc::new(AtomicUsize::new(0));
            let mut set = pin!(InFlight::<_, 4>::new());
            set.as_mut().push(Countdown {
                polls: polls.clone(),
                ready_on: 3,
                output: 7,
            });
            set.as_mut().push(Countdown {
                polls: Arc::new(AtomicUsize::new(0)),
                ready_on: 1,
                output: 9,
            });

            let mut outputs = alloc::vec::Vec::new();
            while let Some(output) = next_of(set.as_mut()).await {
                outputs.push(output);
            }
            outputs.sort_unstable();
            assert_eq!(outputs.as_slice(), &[7, 9]);
            assert_eq!(set.len(), 0);
            assert!(
                polls.load(Ordering::Acquire) >= 3,
                "a pending future is re-polled until it completes"
            );
        });
    }

    /// A slot a completed future left takes the next `push`, which is
    /// what keeps a pump feeding a ring of fixed depth.
    #[test]
    fn a_finished_slot_takes_another_future() {
        block_on(async {
            let mut set = pin!(InFlight::<_, 2>::new());
            set.as_mut().push(ready(1_u32));
            assert_eq!(next_of(set.as_mut()).await, Some(1));
            assert!(set.is_empty());

            set.as_mut().push(ready(2_u32));
            set.as_mut().push(ready(3_u32));
            assert_eq!(next_of(set.as_mut()).await, Some(2));
            assert_eq!(next_of(set.as_mut()).await, Some(3));
            assert_eq!(next_of(set.as_mut()).await, None);
        });
    }

    /// `N` is the whole of the capacity: the `N + 1`th push is refused
    /// loudly rather than spilled somewhere.
    #[test]
    #[should_panic(expected = "an InFlight is pushed only while a slot is free")]
    fn a_full_set_refuses_another_future() {
        let mut set = pin!(InFlight::<_, 2>::new());
        set.as_mut().push(pending::<()>());
        set.as_mut().push(pending::<()>());
        set.as_mut().push(pending::<()>());
    }

    /// The set is its futures' home in place: dropping it while a
    /// future is still outstanding drops the future where it sat.
    #[test]
    fn dropping_the_set_drops_what_it_holds() {
        let dropped = Arc::new(AtomicBool::new(false));
        {
            let mut set = pin!(InFlight::<_, 2>::new());
            set.as_mut().push(Dropped(dropped.clone()));
            block_on(async {
                // One poll, so the future is genuinely in flight rather
                // than merely stored.
                assert_eq!(
                    poll_once(next_of(set.as_mut())).await,
                    None,
                    "an outstanding future leaves the set pending"
                );
            });
        }
        assert!(
            dropped.load(Ordering::Acquire),
            "the set's drop dropped the pinned future in place"
        );
    }

    /// Futures live in the set, not behind pointers: the size of the
    /// whole is the size of the slots plus the count. A heap would show
    /// up here as a pointer, not as the futures themselves.
    #[test]
    fn the_set_is_its_slots_and_nothing_else() {
        assert_eq!(
            core::mem::size_of::<InFlight<u32, 4>>(),
            4 * core::mem::size_of::<Option<u32>>() + core::mem::size_of::<usize>()
        );
    }
}
