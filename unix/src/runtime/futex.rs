extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use core::task::{Context, Poll};

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use spin::Mutex;
use triomphe::Arc;

use helios_sync::{Notify, NotifyWaiter};

const FUTEX_TABLE_INITIAL_CAPACITY: usize = 64;

/// Bitset that matches every bit in `FUTEX_WAIT_BITSET` and
/// `FUTEX_WAKE_BITSET` (`include/uapi/linux/futex.h` `FUTEX_BITSET_MATCH_ANY`).
pub const FUTEX_BITSET_MATCH_ANY: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessMemoryIdentity(u64);

impl ProcessMemoryIdentity {
    pub const fn new(raw: u64) -> Self {
        assert!(raw != 0, "process memory identity zero is reserved");
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GuestAddress(u64);

impl GuestAddress {
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The address a wasm32 guest passed as a pointer.
    pub const fn wasm32(raw: u32) -> Self {
        Self(raw as u64)
    }

    /// The address `bytes` past this one, for a field of a guest structure or
    /// an element of a guest array.
    ///
    /// The sum is computed in 64 bits and saturates, so it never wraps back
    /// into low guest memory. An address beyond the 32-bit guest space is
    /// representable and lies outside every guest memory, so the access that
    /// uses it fails with `EFAULT` at the host-call memory helpers.
    #[must_use]
    pub const fn offset(self, bytes: u64) -> Self {
        Self(self.0.saturating_add(bytes))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Whether a futex operation names a process-private or shared queue.
///
/// Linux's `get_futex_key` (`kernel/futex/core.c`) tags shared keys on
/// anonymous memory with `FUT_OFF_MMSHARED` and leaves private keys
/// untagged, so `FUTEX_PRIVATE_FLAG` and shared operations on the same
/// address use distinct queues.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FutexScope {
    #[default]
    Private,
    Shared,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FutexKey {
    memory: ProcessMemoryIdentity,
    address: GuestAddress,
    scope: FutexScope,
}

impl FutexKey {
    pub const fn new(memory: ProcessMemoryIdentity, address: GuestAddress) -> Self {
        Self::with_scope(memory, address, FutexScope::Private)
    }

    pub const fn shared(memory: ProcessMemoryIdentity, address: GuestAddress) -> Self {
        Self::with_scope(memory, address, FutexScope::Shared)
    }

    pub const fn with_scope(
        memory: ProcessMemoryIdentity,
        address: GuestAddress,
        scope: FutexScope,
    ) -> Self {
        Self {
            memory,
            address,
            scope,
        }
    }

    pub const fn memory(self) -> ProcessMemoryIdentity {
        self.memory
    }

    pub const fn address(self) -> GuestAddress {
        self.address
    }

    pub const fn scope(self) -> FutexScope {
        self.scope
    }
}

/// Counts of waiters woken and moved by a requeue operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RequeueOutcome {
    pub woken: usize,
    pub requeued: usize,
}

/// One queued wait, shared by its registration and its current key's queue.
///
/// A wake releases a specific waiter: it takes the waiter off its key's
/// queue, sets `released`, and broadcasts on the waiter's own signal. The
/// signal belongs to this one wait and is only ever broadcast, once, so
/// it carries no permit for anyone else to claim. `key` tracks requeues
/// and is mutated under `FutexTable::entries` (lock order: `entries` ->
/// `waiter.key`).
struct FutexWaiter {
    key: Mutex<FutexKey>,
    bitset: u32,
    /// Written once, by the wake that dequeues the waiter, under the
    /// table lock and before the broadcast.
    released: AtomicBool,
    signal: Notify,
}

impl FutexWaiter {
    fn new(key: FutexKey, bitset: u32) -> Self {
        Self {
            key: Mutex::new(key),
            bitset,
            released: AtomicBool::new(false),
            signal: Notify::new(),
        }
    }

    fn release(&self) {
        self.released.store(true, Ordering::Release);
        self.signal.notify_all();
    }
}

/// One live futex wait, queued on its key until a wake releases it or
/// this value is dropped.
///
/// The wait is queued, and its release armed, when it is created, so a
/// caller registers before it compares the guest word and a wake that
/// lands between the comparison and the park still releases it.
///
/// Dropping an unreleased registration takes it off its current key's queue
/// (including after a requeue) and removes the key once its queue is empty,
/// so a wait whose future is cancelled while parked (a signal ending the
/// thread, a kill, the store's teardown) leaves nothing behind. A released
/// registration is already off the queue. It owns a handle to the table it
/// is queued in; the drop takes the table's lock, which is never held across
/// an `.await` anywhere.
pub struct FutexWaitRegistration {
    table: FutexTable,
    waiter: Arc<FutexWaiter>,
    wait: NotifyWaiter,
}

impl FutexWaitRegistration {
    pub fn key(&self) -> FutexKey {
        *self.waiter.key.lock()
    }

    /// Whether a wake has released this wait.
    ///
    /// A wait that ends another way (a timeout, a word that no longer
    /// matches) reads this to learn whether a wake counted it anyway.
    pub fn is_released(&self) -> bool {
        self.waiter.released.load(Ordering::Acquire)
    }

    /// Polls until a wake releases this wait.
    pub fn poll_released(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            if self.is_released() {
                return Poll::Ready(());
            }
            match self.waiter.signal.poll_notified(cx, &mut self.wait) {
                Poll::Ready(()) => continue,
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    /// Resolves once a wake releases this wait.
    pub async fn released(&mut self) {
        core::future::poll_fn(|cx| self.poll_released(cx)).await;
    }
}

impl Drop for FutexWaitRegistration {
    fn drop(&mut self) {
        let mut entries = self.table.entries.lock();
        // Read under the lock a wake releases under: a waiter either is
        // still queued here or was dequeued by the wake that released it.
        if self.waiter.released.load(Ordering::Relaxed) {
            return;
        }
        let key = *self.waiter.key.lock();
        match entries.entry(key) {
            Entry::Occupied(mut entry) => {
                let queue = entry.get_mut();
                let index = queue
                    .iter()
                    .position(|queued| Arc::ptr_eq(queued, &self.waiter))
                    .expect("an unreleased futex wait is queued on its key");
                queue.remove(index);
                if queue.is_empty() {
                    entry.remove();
                }
            }
            Entry::Vacant(_) => panic!("an unreleased futex wait referenced an unknown key"),
        }
    }
}

/// The machine's futex waits, shared by every wait that is queued in it.
///
/// Each key holds its unreleased waiters in arrival order. Cloning yields
/// another handle to the same table. Concurrency contract: every operation
/// takes the table's spin lock for a bounded, non-awaiting critical
/// section; a wake broadcasts to the waiters it releases inside it, which
/// is lock-free and never blocks.
pub struct FutexTable {
    entries: Arc<Mutex<HashMap<FutexKey, VecDeque<Arc<FutexWaiter>>>>>,
}

impl Default for FutexTable {
    fn default() -> Self {
        Self::new()
    }
}

impl FutexTable {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Mutex::new(HashMap::with_capacity(
                FUTEX_TABLE_INITIAL_CAPACITY,
            ))),
        }
    }

    /// Queues a wait matching every bitset bit on `key` until a wake
    /// releases it or the returned registration is dropped.
    pub fn prepare_wait(&self, key: FutexKey) -> FutexWaitRegistration {
        self.prepare_wait_bitset(key, FUTEX_BITSET_MATCH_ANY)
    }

    /// Queues a wait on `key` with the non-zero bitmask `bitset`.
    pub fn prepare_wait_bitset(&self, key: FutexKey, bitset: u32) -> FutexWaitRegistration {
        assert!(bitset != 0, "futex wait bitset must be nonzero");
        let waiter = Arc::new(FutexWaiter::new(key, bitset));
        // Armed before the waiter is queued, so no wake can precede it.
        let wait = waiter.signal.waiter();
        self.entries
            .lock()
            .entry(key)
            .or_default()
            .push_back(waiter.clone());
        FutexWaitRegistration {
            table: self.clone(),
            waiter,
            wait,
        }
    }

    /// Releases up to `count` of the key's waiters, oldest first, and
    /// returns how many it released.
    ///
    /// A released waiter leaves the queue at once, so a later wake on
    /// the key, made before the released waiters have run, never counts
    /// them again, and a wait queued after it is not released by it.
    pub fn wake(&self, key: FutexKey, count: usize) -> usize {
        self.wake_bitset(key, count, FUTEX_BITSET_MATCH_ANY)
    }

    /// Releases up to `count` waiters on `key` whose wait bitmask intersects
    /// the non-zero `bitset`, oldest first, and returns how many it released.
    pub fn wake_bitset(&self, key: FutexKey, count: usize, bitset: u32) -> usize {
        assert!(bitset != 0, "futex wake bitset must be nonzero");
        if count == 0 {
            return 0;
        }
        let mut entries = self.entries.lock();
        let Entry::Occupied(mut entry) = entries.entry(key) else {
            return 0;
        };
        let queue = entry.get_mut();
        let mut released = 0usize;
        let mut index = 0usize;
        while index < queue.len() && released < count {
            if queue[index].bitset & bitset != 0 {
                let waiter = queue.remove(index).expect("index is within queue bounds");
                waiter.release();
                released += 1;
            } else {
                index += 1;
            }
        }
        if queue.is_empty() {
            entry.remove();
        }
        released
    }

    /// Releases every waiter queued on the key and returns how many it
    /// released. A wait queued after it parks, exactly as a futex
    /// requires.
    pub fn wake_all(&self, key: FutexKey) -> usize {
        // Released under the lock, which is what a registration's drop
        // relies on to find an unreleased waiter still queued.
        let mut entries = self.entries.lock();
        let Some(queue) = entries.remove(&key) else {
            return 0;
        };
        for waiter in &queue {
            waiter.release();
        }
        queue.len()
    }

    /// Releases up to `wake_count` waiters on `source` and moves up to
    /// `requeue_count` remaining waiters from `source` to the tail of
    /// `target`.
    pub fn requeue(
        &self,
        source: FutexKey,
        target: FutexKey,
        wake_count: usize,
        requeue_count: usize,
    ) -> RequeueOutcome {
        self.requeue_if(source, target, wake_count, requeue_count, || {
            Ok::<(), ()>(())
        })
        .expect("infallible requeue check")
    }

    /// Runs `check` while holding the futex table lock and, when `Ok(())`,
    /// releases up to `wake_count` waiters on `source` and moves up to
    /// `requeue_count` remaining waiters from `source` to `target`.
    pub fn requeue_if<E, F>(
        &self,
        source: FutexKey,
        target: FutexKey,
        wake_count: usize,
        requeue_count: usize,
        check: F,
    ) -> Result<RequeueOutcome, E>
    where
        F: FnOnce() -> Result<(), E>,
    {
        let mut entries = self.entries.lock();
        check()?;
        if source == target {
            let Entry::Occupied(mut entry) = entries.entry(source) else {
                return Ok(RequeueOutcome::default());
            };
            let queue = entry.get_mut();
            let woken = wake_count.min(queue.len());
            for waiter in queue.drain(..woken) {
                waiter.release();
            }
            if queue.is_empty() {
                entry.remove();
            }
            return Ok(RequeueOutcome { woken, requeued: 0 });
        }

        let mut moved = Vec::new();
        let woken = match entries.entry(source) {
            Entry::Vacant(_) => return Ok(RequeueOutcome::default()),
            Entry::Occupied(mut entry) => {
                let queue = entry.get_mut();
                let woken = wake_count.min(queue.len());
                for waiter in queue.drain(..woken) {
                    waiter.release();
                }
                let move_len = requeue_count.min(queue.len());
                moved.extend(queue.drain(..move_len));
                if queue.is_empty() {
                    entry.remove();
                }
                woken
            }
        };
        let requeued = moved.len();
        if requeued > 0 {
            let target_queue = entries.entry(target).or_default();
            for waiter in moved {
                *waiter.key.lock() = target;
                target_queue.push_back(waiter);
            }
        }
        Ok(RequeueOutcome { woken, requeued })
    }

    /// The number of keys with at least one queued wait.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.entries.lock().capacity()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

impl Clone for FutexTable {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use core::task::{Context, Waker};

    use futures_lite::future::{block_on, poll_once};

    use super::*;

    fn key(memory: u64, address: u64) -> FutexKey {
        FutexKey::new(
            ProcessMemoryIdentity::new(memory),
            GuestAddress::new(address),
        )
    }

    #[test]
    fn guest_address_offset_does_not_wrap_the_32_bit_guest_space() {
        let top = GuestAddress::wasm32(u32::MAX - 2);
        assert_eq!(top.offset(4), GuestAddress::new(u64::from(u32::MAX) + 2));
        assert_eq!(GuestAddress::new(u64::MAX - 1).offset(8).raw(), u64::MAX);
    }

    #[test]
    fn wake_on_key_without_waiters_returns_zero_and_inserts_nothing() {
        let table = FutexTable::new();
        assert_eq!(table.wake(key(1, 0x1000), 1), 0);
        assert_eq!(table.wake_all(key(1, 0x1000)), 0);
        assert!(table.is_empty());
    }

    #[test]
    fn wake_releases_up_to_the_requested_count_in_fifo_order() {
        let table = FutexTable::new();
        let mut first = table.prepare_wait(key(1, 0x1000));
        let mut second = table.prepare_wait(key(1, 0x1000));
        let mut third = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake(key(1, 0x1000), 2), 2);

        block_on(first.released());
        block_on(second.released());
        assert!(block_on(poll_once(third.released())).is_none());
        assert_eq!(table.wake(key(1, 0x1000), 2), 1);
        block_on(third.released());
    }

    #[test]
    fn wake_all_releases_every_queued_wait() {
        let table = FutexTable::new();
        let mut first = table.prepare_wait(key(1, 0x1000));
        let mut second = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake_all(key(1, 0x1000)), 2);

        block_on(first.released());
        block_on(second.released());
    }

    #[test]
    fn a_parked_wait_is_woken_by_its_release() {
        let table = FutexTable::new();
        let parked = table.prepare_wait(key(1, 0x1000));
        let mut parked = core::pin::pin!(async move {
            let mut parked = parked;
            parked.released().await;
        });

        assert!(
            block_on(poll_once(parked.as_mut())).is_none(),
            "the wait parks"
        );
        assert_eq!(table.wake(key(1, 0x1000), 1), 1);
        assert!(
            block_on(poll_once(parked.as_mut())).is_some(),
            "the release completes the parked wait"
        );
    }

    #[test]
    fn a_wait_that_follows_wake_all_still_parks() {
        let table = FutexTable::new();
        let mut earlier = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake_all(key(1, 0x1000)), 1);

        let mut later = table.prepare_wait(key(1, 0x1000));
        block_on(earlier.released());
        assert!(
            block_on(poll_once(later.released())).is_none(),
            "a futex wait queued after the wake has nothing to complete it"
        );
    }

    #[test]
    fn a_wake_after_wake_all_releases_nobody_and_a_later_wait_parks() {
        let table = FutexTable::new();
        let mut first = table.prepare_wait(key(1, 0x1000));
        let mut second = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake_all(key(1, 0x1000)), 2);
        assert_eq!(
            table.wake(key(1, 0x1000), 1),
            0,
            "both waiters were already released"
        );

        let mut later = table.prepare_wait(key(1, 0x1000));
        assert!(
            block_on(poll_once(later.released())).is_none(),
            "a wait made after the wakes parks"
        );

        block_on(first.released());
        block_on(second.released());
    }

    #[test]
    fn a_wait_dropped_while_parked_dequeues_itself() {
        let table = FutexTable::new();
        let mut registration = table.prepare_wait(key(1, 0x1000));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            registration.poll_released(&mut cx).is_pending(),
            "the wait parks"
        );
        assert_eq!(table.len(), 1);

        drop(registration);

        assert_eq!(table.len(), 0, "the entry leaves with its last wait");
        assert_eq!(table.wake(key(1, 0x1000), 1), 0, "nobody is parked");
        assert_eq!(table.wake_all(key(1, 0x1000)), 0, "nobody is parked");

        let mut next = table.prepare_wait(key(1, 0x1000));
        assert!(
            block_on(poll_once(next.released())).is_none(),
            "a new wait parks"
        );
        drop(next);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn a_released_wait_drops_without_touching_its_key() {
        let table = FutexTable::new();
        let released = table.prepare_wait(key(1, 0x1000));
        assert_eq!(table.wake(key(1, 0x1000), 1), 1);
        let queued = table.prepare_wait(key(1, 0x1000));

        drop(released);

        assert_eq!(table.len(), 1);
        assert_eq!(table.wake(key(1, 0x1000), 2), 1);
        assert!(queued.is_released());
        drop(queued);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn a_dropped_wait_leaves_the_other_waits_on_its_key_queued() {
        let table = FutexTable::new();
        let kept = table.prepare_wait(key(1, 0x1000));
        let dropped = table.prepare_wait(key(1, 0x1000));

        drop(dropped);

        assert_eq!(table.len(), 1);
        assert_eq!(table.wake(key(1, 0x1000), 2), 1);
        drop(kept);
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn wake_bitset_filters_by_matching_bits_in_fifo_order() {
        let table = FutexTable::new();
        let mut first = table.prepare_wait_bitset(key(1, 0x1000), 0b0010);
        let mut second = table.prepare_wait_bitset(key(1, 0x1000), 0b0100);
        let mut third = table.prepare_wait_bitset(key(1, 0x1000), 0b0011);

        assert_eq!(table.wake_bitset(key(1, 0x1000), 1, 0b0100), 1);
        assert!(!first.is_released());
        block_on(second.released());
        assert!(!third.is_released());

        assert_eq!(table.wake_bitset(key(1, 0x1000), 2, 0b0010), 2);
        block_on(first.released());
        block_on(third.released());
        assert!(table.is_empty());
    }

    #[test]
    fn requeue_wakes_head_and_moves_tail_to_target_queue() {
        let table = FutexTable::new();
        let src = key(1, 0x1000);
        let dst = key(1, 0x2000);
        let mut w1 = table.prepare_wait(src);
        let mut w2 = table.prepare_wait(src);
        let w3 = table.prepare_wait(src);

        let outcome = table.requeue(src, dst, 1, 2);
        assert_eq!(
            outcome,
            RequeueOutcome {
                woken: 1,
                requeued: 2
            }
        );
        block_on(w1.released());
        assert!(!w2.is_released());
        assert_eq!(w2.key(), dst);
        assert_eq!(w3.key(), dst);

        drop(w3);
        assert_eq!(table.wake(dst, 2), 1);
        block_on(w2.released());
        assert!(table.is_empty());
    }

    #[test]
    fn private_and_shared_scopes_at_same_address_are_independent() {
        let table = FutexTable::new();
        let memory = ProcessMemoryIdentity::new(1);
        let addr = GuestAddress::new(0x3000);
        let priv_key = FutexKey::new(memory, addr);
        let shared_key = FutexKey::shared(memory, addr);

        let mut priv_reg = table.prepare_wait(priv_key);
        let mut shared_reg = table.prepare_wait(shared_key);

        assert_eq!(table.wake(priv_key, 1), 1);
        block_on(priv_reg.released());
        assert!(!shared_reg.is_released());

        assert_eq!(table.wake(shared_key, 1), 1);
        block_on(shared_reg.released());
        assert!(table.is_empty());
    }
}
