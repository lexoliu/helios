extern crate alloc;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
use spin::Mutex;
use triomphe::Arc;

use crate::Notify;

const FUTEX_TABLE_INITIAL_CAPACITY: usize = 64;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FutexKey {
    memory: ProcessMemoryIdentity,
    address: GuestAddress,
}

impl FutexKey {
    pub const fn new(memory: ProcessMemoryIdentity, address: GuestAddress) -> Self {
        Self { memory, address }
    }

    pub const fn memory(self) -> ProcessMemoryIdentity {
        self.memory
    }

    pub const fn address(self) -> GuestAddress {
        self.address
    }
}

/// One live futex wait, counted on its key's entry for exactly as long
/// as this value exists.
///
/// Dropping it uncounts the wait and removes the entry at zero, so a wait
/// whose future is cancelled while parked (a signal ending the thread, a
/// kill, the store's teardown) leaves nothing behind. It owns a handle to
/// the table it is counted in; the drop takes the table's lock, which is
/// never held across an `.await` anywhere.
pub struct FutexWaitRegistration {
    table: FutexTable,
    key: FutexKey,
    notify: Arc<Notify>,
}

impl FutexWaitRegistration {
    pub fn key(&self) -> FutexKey {
        self.key
    }

    pub fn notify(&self) -> &Notify {
        &self.notify
    }
}

impl Drop for FutexWaitRegistration {
    fn drop(&mut self) {
        let mut entries = self.table.entries.lock();
        match entries.entry(self.key) {
            Entry::Occupied(mut entry) => {
                let futex = entry.get_mut();
                assert!(futex.waiters > 0, "futex wait release underflow");
                futex.waiters -= 1;
                if futex.waiters == 0 {
                    entry.remove();
                }
            }
            Entry::Vacant(_) => panic!("futex wait release referenced an unknown key"),
        }
    }
}

struct FutexEntry {
    notify: Arc<Notify>,
    waiters: usize,
}

/// The machine's futex waits, shared by every wait that is counted in it.
///
/// Cloning yields another handle to the same table. Concurrency contract:
/// every operation takes the table's spin lock for a bounded, non-awaiting
/// critical section; nothing blocks while holding it.
#[derive(Clone)]
pub struct FutexTable {
    entries: Arc<Mutex<HashMap<FutexKey, FutexEntry>>>,
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

    /// Counts a wait on `key` until the returned registration is dropped.
    pub fn prepare_wait(&self, key: FutexKey) -> FutexWaitRegistration {
        let mut entries = self.entries.lock();
        let notify = match entries.entry(key) {
            Entry::Occupied(mut entry) => {
                let futex = entry.get_mut();
                futex.waiters = futex.waiters.saturating_add(1);
                futex.notify.clone()
            }
            Entry::Vacant(entry) => {
                let notify = Arc::new(Notify::new());
                entry.insert(FutexEntry {
                    notify: notify.clone(),
                    waiters: 1,
                });
                notify
            }
        };
        FutexWaitRegistration {
            table: self.clone(),
            key,
            notify,
        }
    }

    /// Wakes at most `count` of the key's waiters.
    ///
    /// The permits are stored, so a waiter that has registered through
    /// [`Self::prepare_wait`] but has not parked yet still claims one.
    pub fn wake(&self, key: FutexKey, count: usize) -> usize {
        let entries = self.entries.lock();
        let Some(entry) = entries.get(&key) else {
            return 0;
        };
        let wake_count = count.min(entry.waiters);
        entry.notify.notify_count(wake_count);
        wake_count
    }

    /// Wakes every waiter parked on the key.
    ///
    /// This is a broadcast and stores no permit: a waiter that arrives
    /// after it has to park, exactly as a futex requires. Waiters arm
    /// their wait before they compare the guest word, which is what
    /// keeps a wake between the comparison and the park from being
    /// lost.
    pub fn wake_all(&self, key: FutexKey) -> usize {
        let entries = self.entries.lock();
        let Some(entry) = entries.get(&key) else {
            return 0;
        };
        entry.notify.notify_all();
        entry.waiters
    }

    /// The number of keys with at least one live wait.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use core::task::{Context, Waker};

    use futures_lite::future::{block_on, poll_once};

    use super::{
        FUTEX_TABLE_INITIAL_CAPACITY, FutexKey, FutexTable, GuestAddress, ProcessMemoryIdentity,
    };

    fn key(memory: u64, address: u64) -> FutexKey {
        FutexKey::new(
            ProcessMemoryIdentity::new(memory),
            GuestAddress::new(address),
        )
    }

    #[test]
    fn guest_address_offset_does_not_wrap_the_32_bit_guest_space() {
        let top = GuestAddress::wasm32(u32::MAX - 2);

        assert_eq!(top.offset(4).raw(), u64::from(u32::MAX) + 2);
        assert_eq!(GuestAddress::new(u64::MAX).offset(1).raw(), u64::MAX);
    }

    #[test]
    fn futex_table_preallocates_expected_wait_keys() {
        let table = FutexTable::new();

        assert!(table.entries.lock().capacity() >= FUTEX_TABLE_INITIAL_CAPACITY);
    }

    #[test]
    fn wake_one_targets_matching_key() {
        let table = FutexTable::new();
        let registration = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake(key(1, 0x2000), 1), 0);
        assert_eq!(table.wake(key(1, 0x1000), 1), 1);

        block_on(registration.notify().notified());
    }

    #[test]
    fn process_memory_identity_partitions_same_guest_address() {
        let table = FutexTable::new();
        let left = table.prepare_wait(key(1, 0x1000));
        let right = table.prepare_wait(key(2, 0x1000));

        assert_eq!(table.wake(key(1, 0x1000), 1), 1);
        assert_eq!(table.wake(key(2, 0x1000), 1), 1);

        block_on(left.notify().notified());
        block_on(right.notify().notified());
    }

    #[test]
    fn wake_all_reports_all_waiters_for_key() {
        let table = FutexTable::new();
        let first = table.prepare_wait(key(1, 0x1000));
        let second = table.prepare_wait(key(1, 0x1000));
        // Both waits are armed before the wake, as the guest-facing wait
        // path arms before it compares the futex word.
        let first_wait = first.notify().notified();
        let second_wait = second.notify().notified();

        assert_eq!(table.wake_all(key(1, 0x1000)), 2);

        block_on(first_wait);
        block_on(second_wait);
    }

    /// A wake-all is a broadcast, not a permit bank: a waiter that
    /// arrives after it has to park, or every futex wait that follows a
    /// wake on the same key returns without ever blocking.
    #[test]
    fn a_wait_that_follows_wake_all_still_parks() {
        let table = FutexTable::new();
        let registration = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake_all(key(1, 0x1000)), 1);

        assert!(
            block_on(poll_once(registration.notify().notified())).is_none(),
            "a futex wait armed after the wake has nothing to consume"
        );
    }

    /// A wait whose future is dropped while parked leaves no count behind:
    /// a wake finds nobody, banks no permit for the next wait, and the
    /// table is empty once no wait is alive.
    #[test]
    fn a_wait_dropped_while_parked_uncounts_itself() {
        let table = FutexTable::new();
        let registration = table.prepare_wait(key(1, 0x1000));
        let mut wake = registration.notify().waiter();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(
            registration
                .notify()
                .poll_notified(&mut cx, &mut wake)
                .is_pending(),
            "the wait parks"
        );
        assert_eq!(table.len(), 1);

        drop(registration);

        assert_eq!(table.len(), 0, "the entry leaves with its last wait");
        assert_eq!(table.wake(key(1, 0x1000), 1), 0, "nobody is parked");
        assert_eq!(table.wake_all(key(1, 0x1000)), 0, "nobody is parked");

        let next = table.prepare_wait(key(1, 0x1000));
        assert!(
            block_on(poll_once(next.notify().notified())).is_none(),
            "a new wait parks instead of claiming a stale permit"
        );
        drop(next);
        assert_eq!(table.len(), 0);
    }

    /// Only the dropped wait is uncounted: the entry stays while another
    /// wait on the key is alive.
    #[test]
    fn a_dropped_wait_leaves_the_other_waits_on_its_key_counted() {
        let table = FutexTable::new();
        let kept = table.prepare_wait(key(1, 0x1000));
        let dropped = table.prepare_wait(key(1, 0x1000));

        drop(dropped);

        assert_eq!(table.len(), 1);
        assert_eq!(table.wake(key(1, 0x1000), 2), 1);
        drop(kept);
        assert_eq!(table.len(), 0);
    }
}
