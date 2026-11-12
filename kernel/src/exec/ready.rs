//! Ready queues whose capacity is the tasks' own storage.
//!
//! A task is an async-task task: one heap block holding a header, the
//! schedule closure and the future. The header carries a metadata slot
//! (`Builder::metadata`), and the executor fills it with a [`ReadyLink`],
//! the `next` pointer of an intrusive list. A queued task is therefore
//! linked through memory it already owns, a push has nothing to allocate
//! and nothing to run out of, and there is no capacity to exhaust. The
//! shape is Linux's `llist` (`llist_add` / `llist_del_all`, the remote
//! wake-up list), Embassy's run queue and Tokio's inject queue.
//!
//! # Concurrency contract
//!
//! | Operation | Who | Context | Progress | Allocates |
//! | --- | --- | --- | --- | --- |
//! | [`ReadyInbox::push`] | any processor | task, interrupt handler, drop path | lock-free: a CAS retries only when another push or a take-all succeeded | never |
//! | [`ReadyInbox::take_all`], then [`RunList::append_reversed`] and [`RunList::pop_front`] on a local queue | the owner's `Executor` only; its `RunList` is `!Sync` | run loop, never an interrupt handler | wait-free swap, then owner-private work | never |
//! | [`GlobalReadyQueue::pop`] | any processor's run loop | never an interrupt handler | `try_lock`, never spins: a loser reports [`GlobalPop::Contended`] and its count stays non-zero, so the idle handshake re-tests and never parks on queued work | never |
//!
//! Cache lines: each `CachePadded<ReadyShared>` (inbox head and ready
//! count) is written by every pusher of that queue and by its consumer's
//! swap and decrement; the two words share a line because a push writes
//! both. The [`GlobalReadyQueue`] consumer block is written by whichever
//! processor wins its `try_lock`. A local queue's [`RunList`] is owner
//! memory inside the `Executor`.
//!
//! Interrupt safety: a push interrupted mid-CAS by a handler that pushes
//! to the same inbox fails its CAS and retries. No interrupt handler
//! takes the global consumer lock or touches a [`RunList`], so the
//! `IrqSafeMutex` rule for locks an interrupt handler can reach does not
//! apply; the lock is a plain spin word used only through `try_lock`.
//!
//! At most once: a queue takes a [`Runnable`] by value and stores its
//! ownership as the raw task pointer. async-task issues at most one
//! `Runnable` per task, so a link is written only by the unique holder of
//! that task's token and a node is on at most one list, once.
//!
//! ABA: the only removal from an inbox is `swap(null)` of the whole
//! chain. A push's CAS succeeds only if `head` still equals the value it
//! wrote into its own `next`; if that node was taken and pushed again in
//! between, `next` still names the current head and the chain is
//! consistent.
//!
//! Memory ordering: a push's link store happens before its `Release`
//! CAS. The consumer's `Acquire` swap reads the newest push, and every
//! earlier successful CAS on `head` is a read-modify-write in the same
//! release sequence, so the swap synchronizes with every push in the
//! chain and sees every `next`. A node the consumer owns is read and
//! written by one thread at a time, and a hand-off between processors
//! (the global list under its lock) is ordered by the lock.
//!
//! Reclamation: a pusher dereferences only its own node. A node is
//! reachable only while its `Runnable` is owned by an inbox chain or a
//! [`RunList`], and that ownership keeps the task allocation alive; the
//! allocation is freed only after the consumer has turned the node back
//! into a `Runnable` and run or dropped it.
//!
//! Order: each queue is strict FIFO in push-linearization order. A
//! take-all takes every node pushed before the swap, the reversal
//! restores push order, and the next batch is appended behind it.

use core::cell::Cell;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use async_task::Runnable;
use crossbeam_utils::CachePadded;
use spin::Mutex;

/// The ready-list link every executor task carries in its async-task
/// header (`Builder::metadata`).
///
/// Holds the raw task pointer of the next queued task, null at the end
/// of a list. Written only by the holder of this task's [`Runnable`].
pub struct ReadyLink {
    next: AtomicPtr<()>,
}

impl ReadyLink {
    pub(crate) const fn new() -> Self {
        Self {
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

/// The token every ready queue holds.
pub(crate) type ReadyRunnable = Runnable<ReadyLink>;

/// Runs `with` on the link of the task `node` names, and leaves the task
/// owned by `node`.
///
/// # Safety
///
/// `node` came from [`Runnable::into_raw`] of a [`ReadyRunnable`] and has
/// not been passed to [`Runnable::from_raw`] since: the caller holds that
/// task's one token as a raw pointer (the "at most once" invariant in the
/// module docs), and `with` must not unwind.
unsafe fn with_link<R>(node: NonNull<()>, with: impl FnOnce(&ReadyLink) -> R) -> R {
    // SAFETY: the caller owns the one `into_raw` this `from_raw` consumes,
    // and the `into_raw` below gives that ownership back to the caller.
    let runnable = unsafe { ReadyRunnable::from_raw(node) };
    let result = with(runnable.metadata());
    let returned = runnable.into_raw();
    debug_assert_eq!(returned, node);
    result
}

/// Lock-free multi-producer push, take-all consumer (`llist`,
/// Embassy's `TransferStack`).
pub(crate) struct ReadyInbox {
    head: AtomicPtr<()>,
}

impl ReadyInbox {
    pub(crate) const fn new() -> Self {
        Self {
            head: AtomicPtr::new(ptr::null_mut()),
        }
    }

    /// Queues `runnable`. Any processor, any context, interrupt handlers
    /// included; lock-free, allocation-free and infallible.
    pub(crate) fn push(&self, runnable: ReadyRunnable) {
        let link: *const ReadyLink = runnable.metadata();
        let node = runnable.into_raw();
        // SAFETY: this push owns `node`'s one token until the CAS below
        // succeeds, and the token keeps the task allocation, and so its
        // header metadata, alive (the reclamation invariant). The link is
        // not touched after the CAS publishes the node.
        let link = unsafe { &*link };
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            link.next.store(head, Ordering::Relaxed);
            // `Release`: the link store above is visible to the consumer's
            // `Acquire` swap (the memory-ordering invariant). The push
            // never dereferences `head`.
            match self.head.compare_exchange_weak(
                head,
                node.as_ptr(),
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(current) => head = current,
            }
        }
    }

    /// Every node pushed before the swap, newest first.
    pub(crate) fn take_all(&self) -> ReadyChain {
        // An empty inbox is answered without a write, so an owner that
        // polls it on every pass does not pull the pushers' line
        // exclusive (Embassy's `definitely_empty`).
        if self.head.load(Ordering::Relaxed).is_null() {
            return ReadyChain(None);
        }
        // `Acquire`: pairs with every push's `Release` CAS.
        ReadyChain(NonNull::new(
            self.head.swap(ptr::null_mut(), Ordering::Acquire),
        ))
    }
}

impl Drop for ReadyInbox {
    fn drop(&mut self) {
        drop(self.take_all());
    }
}

/// An owned, newest-first chain taken from a [`ReadyInbox`]. Dropping it
/// cancels every task still on it, once each.
pub(crate) struct ReadyChain(Option<NonNull<()>>);

impl Drop for ReadyChain {
    fn drop(&mut self) {
        let mut node = self.0.take();
        while let Some(current) = node {
            // SAFETY: the chain owns every node on it, one token each
            // (the at-most-once invariant), and this is the one
            // `from_raw` for `current`: the walk moves on before the
            // runnable drops and never revisits a node.
            let runnable = unsafe { ReadyRunnable::from_raw(current) };
            node = NonNull::new(runnable.metadata().next.load(Ordering::Relaxed));
            drop(runnable);
        }
    }
}

/// A FIFO of owned tasks linked through the same `next` field.
///
/// Methods take `&self`; the cells make the holder `!Sync`, so one
/// thread drives a list at a time. A local queue's list lives in its
/// `Executor`; the global queue's lives behind a lock.
pub(crate) struct RunList {
    head: Cell<Option<NonNull<()>>>,
    tail: Cell<Option<NonNull<()>>>,
}

// SAFETY: a list owns the `Runnable`s it links, one token per node, and a
// `Runnable<ReadyLink>` is `Send` (`ReadyLink` is `Send + Sync`). Moving
// the list moves that ownership, and nothing else aliases the nodes.
unsafe impl Send for RunList {}

impl RunList {
    pub(crate) const fn new() -> Self {
        Self {
            head: Cell::new(None),
            tail: Cell::new(None),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.head.get().is_none()
    }

    /// Reverses `chain` into push order (`llist_reverse_order`) and
    /// appends it behind everything already listed.
    pub(crate) fn append_reversed(&self, mut chain: ReadyChain) {
        // Taking the head leaves the chain empty, so its `Drop` has
        // nothing to cancel.
        let Some(newest) = chain.0.take() else {
            return;
        };
        let mut oldest: Option<NonNull<()>> = None;
        let mut node = Some(newest);
        while let Some(current) = node {
            // SAFETY: the chain owned `current`'s one token and this list
            // now does; the closure only touches atomics.
            node = unsafe {
                with_link(current, |link| {
                    let next = NonNull::new(link.next.load(Ordering::Relaxed));
                    link.next.store(
                        oldest.map_or(ptr::null_mut(), NonNull::as_ptr),
                        Ordering::Relaxed,
                    );
                    next
                })
            };
            oldest = Some(current);
        }
        match self.tail.get() {
            // SAFETY: the tail is a node this list owns.
            Some(tail) => unsafe {
                with_link(tail, |link| {
                    link.next.store(
                        oldest.map_or(ptr::null_mut(), NonNull::as_ptr),
                        Ordering::Relaxed,
                    );
                });
            },
            None => self.head.set(oldest),
        }
        // The newest push is now the last node, and its `next` is null.
        self.tail.set(Some(newest));
    }

    /// The oldest task listed, as an owned token.
    pub(crate) fn pop_front(&self) -> Option<ReadyRunnable> {
        let head = self.head.get()?;
        // SAFETY: the list owns `head`'s one token and hands it to the
        // caller here, once: `head` is unlinked below.
        let runnable = unsafe { ReadyRunnable::from_raw(head) };
        let next = NonNull::new(runnable.metadata().next.load(Ordering::Relaxed));
        self.head.set(next);
        if next.is_none() {
            self.tail.set(None);
        }
        Some(runnable)
    }

    /// Hands every listed task back to `inbox`, oldest first.
    pub(crate) fn drain_into(&self, inbox: &ReadyInbox) {
        while let Some(runnable) = self.pop_front() {
            inbox.push(runnable);
        }
    }
}

impl Drop for RunList {
    fn drop(&mut self) {
        while let Some(runnable) = self.pop_front() {
            drop(runnable);
        }
    }
}

/// The half of a ready queue every pusher writes: the inbox head and the
/// count of tasks queued anywhere in the queue (inbox or list).
///
/// The count is the sender half of the idle handshake (`idle`'s module
/// docs): a push increments it before it publishes the task, and the
/// consumer decrements it only once the task is out of the queue.
pub(crate) struct ReadyShared {
    pub(crate) inbox: ReadyInbox,
    pub(crate) ready: AtomicUsize,
}

impl ReadyShared {
    pub(crate) const fn new() -> Self {
        Self {
            inbox: ReadyInbox::new(),
            ready: AtomicUsize::new(0),
        }
    }

    /// Accounts for one task leaving the queue.
    pub(crate) fn release_one(&self) {
        let previous = self.ready.fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "executor ready count underflowed");
    }
}

/// The machine-wide ready queue.
pub(crate) struct GlobalReadyQueue {
    /// Written by every pusher.
    pub(crate) shared: CachePadded<ReadyShared>,
    /// Written by whichever processor wins the `try_lock`.
    consumer: CachePadded<Mutex<RunList>>,
}

/// What a [`GlobalReadyQueue::pop`] found.
pub(crate) enum GlobalPop {
    Ready(ReadyRunnable),
    Empty,
    /// Another processor holds the consumer lock.
    Contended,
}

impl GlobalReadyQueue {
    pub(crate) const fn new() -> Self {
        Self {
            shared: CachePadded::new(ReadyShared::new()),
            consumer: CachePadded::new(Mutex::new(RunList::new())),
        }
    }

    /// Takes the oldest queued task, or reports why there is none. Never
    /// spins on the consumer lock.
    pub(crate) fn pop(&self) -> GlobalPop {
        let Some(list) = self.consumer.try_lock() else {
            return GlobalPop::Contended;
        };
        if list.is_empty() {
            list.append_reversed(self.shared.inbox.take_all());
        }
        match list.pop_front() {
            Some(runnable) => {
                self.shared.release_one();
                GlobalPop::Ready(runnable)
            }
            None => GlobalPop::Empty,
        }
    }
}

#[cfg(test)]
mod tests {
    use core::future::Future;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use std::sync::{Arc, Mutex};
    use std::vec::Vec;

    use async_task::Builder;

    use super::{ReadyInbox, ReadyLink, RunList};

    /// Spawns a detached task whose waker, and first schedule, push onto
    /// `inbox`: the real async-task path the executor's schedulers take.
    fn push_task(inbox: &Arc<ReadyInbox>, future: impl Future<Output = ()> + Send + 'static) {
        let inbox = Arc::clone(inbox);
        let (runnable, task) = Builder::new()
            .metadata(ReadyLink::new())
            .spawn(move |_| future, move |runnable| inbox.push(runnable));
        task.detach();
        runnable.schedule();
    }

    /// Counts how many times its future is dropped.
    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counted_task(inbox: &Arc<ReadyInbox>, drops: &Arc<AtomicUsize>) {
        let guard = DropCount(Arc::clone(drops));
        push_task(inbox, async move {
            let _guard = guard;
            core::future::pending::<()>().await;
        });
    }

    #[test]
    fn a_chain_taken_in_batches_comes_out_in_push_order() {
        let inbox = Arc::new(ReadyInbox::new());
        let list = RunList::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let push = |index: usize| {
            let order = Arc::clone(&order);
            push_task(&inbox, async move {
                order.lock().unwrap().push(index);
            });
        };

        (0..3).for_each(&push);
        list.append_reversed(inbox.take_all());
        (3..7).for_each(&push);
        // A batch taken while an earlier one is still listed goes behind it.
        list.append_reversed(inbox.take_all());
        list.pop_front().expect("task 0 is listed").run();
        (7..10).for_each(&push);
        list.append_reversed(inbox.take_all());
        // An empty take changes nothing.
        list.append_reversed(inbox.take_all());
        while let Some(runnable) = list.pop_front() {
            runnable.run();
        }

        assert!(list.is_empty());
        assert_eq!(*order.lock().unwrap(), (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn every_push_from_many_threads_is_popped_once_and_in_each_producers_order() {
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 100_000;

        let inbox = Arc::new(ReadyInbox::new());
        let next_expected: Arc<[AtomicUsize; PRODUCERS]> =
            Arc::new(core::array::from_fn(|_| AtomicUsize::new(0)));
        let out_of_order = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|scope| {
            for producer in 0..PRODUCERS {
                let inbox = Arc::clone(&inbox);
                let next_expected = Arc::clone(&next_expected);
                let out_of_order = Arc::clone(&out_of_order);
                scope.spawn(move || {
                    for sequence in 0..PER_PRODUCER {
                        let next_expected = Arc::clone(&next_expected);
                        let out_of_order = Arc::clone(&out_of_order);
                        push_task(&inbox, async move {
                            if next_expected[producer].fetch_add(1, Ordering::Relaxed) != sequence {
                                out_of_order.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    }
                });
            }

            // Drain while the producers push.
            let list = RunList::new();
            let mut ran = 0;
            while ran < PRODUCERS * PER_PRODUCER {
                if list.is_empty() {
                    list.append_reversed(inbox.take_all());
                }
                match list.pop_front() {
                    Some(runnable) => {
                        runnable.run();
                        ran += 1;
                    }
                    None => std::thread::yield_now(),
                }
            }
        });

        assert_eq!(out_of_order.load(Ordering::Relaxed), 0);
        for expected in next_expected.iter() {
            assert_eq!(expected.load(Ordering::Relaxed), PER_PRODUCER);
        }
    }

    #[test]
    fn dropping_a_chain_or_a_run_list_cancels_each_task_once() {
        let inbox = Arc::new(ReadyInbox::new());
        let drops = Arc::new(AtomicUsize::new(0));

        for _ in 0..5 {
            counted_task(&inbox, &drops);
        }
        drop(inbox.take_all());
        assert_eq!(drops.load(Ordering::Relaxed), 5);

        for _ in 0..5 {
            counted_task(&inbox, &drops);
        }
        let list = RunList::new();
        list.append_reversed(inbox.take_all());
        drop(list.pop_front().expect("a task is listed"));
        assert_eq!(drops.load(Ordering::Relaxed), 6);
        drop(list);
        assert_eq!(drops.load(Ordering::Relaxed), 10);

        // Nothing is left to cancel a second time.
        drop(inbox.take_all());
        assert_eq!(drops.load(Ordering::Relaxed), 10);
    }
}
