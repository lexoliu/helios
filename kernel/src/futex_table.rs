extern crate alloc;

use hashbrown::HashMap;
use hashbrown::hash_map::Entry;
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

#[derive(Clone)]
pub struct FutexWaitRegistration {
    key: FutexKey,
    notify: Arc<Notify>,
}

impl FutexWaitRegistration {
    pub fn key(&self) -> FutexKey {
        self.key
    }

    pub fn notify(&self) -> Arc<Notify> {
        self.notify.clone()
    }
}

struct FutexEntry {
    notify: Arc<Notify>,
    waiters: usize,
}

#[derive(Default)]
pub struct FutexTable {
    entries: HashMap<FutexKey, FutexEntry>,
}

impl FutexTable {
    pub fn new() -> Self {
        Self {
            entries: HashMap::with_capacity(FUTEX_TABLE_INITIAL_CAPACITY),
        }
    }

    pub fn prepare_wait(&mut self, key: FutexKey) -> FutexWaitRegistration {
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.waiters = entry.waiters.saturating_add(1);
            return FutexWaitRegistration {
                key,
                notify: entry.notify.clone(),
            };
        }

        let notify = Arc::new(Notify::new());
        self.entries.insert(
            key,
            FutexEntry {
                notify: notify.clone(),
                waiters: 1,
            },
        );
        FutexWaitRegistration { key, notify }
    }

    pub fn complete_wait(&mut self, registration: FutexWaitRegistration) {
        match self.entries.entry(registration.key) {
            Entry::Occupied(mut entry) => {
                let futex = entry.get_mut();
                assert!(futex.waiters > 0, "futex wait completion underflow");
                futex.waiters -= 1;
                if futex.waiters == 0 {
                    entry.remove();
                }
            }
            Entry::Vacant(_) => panic!("futex wait completion referenced an unknown key"),
        }
    }

    pub fn wake(&self, key: FutexKey, count: usize) -> usize {
        let Some(entry) = self.entries.get(&key) else {
            return 0;
        };
        let wake_count = count.min(entry.waiters);
        entry.notify.notify_count(wake_count);
        wake_count
    }

    pub fn wake_all(&self, key: FutexKey) -> usize {
        let Some(entry) = self.entries.get(&key) else {
            return 0;
        };
        entry.notify.notify_all();
        entry.waiters
    }
}

#[cfg(test)]
mod tests {
    use futures_lite::future::block_on;

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
    fn futex_table_preallocates_expected_wait_keys() {
        let table = FutexTable::new();

        assert!(table.entries.capacity() >= FUTEX_TABLE_INITIAL_CAPACITY);
    }

    #[test]
    fn wake_one_targets_matching_key() {
        let mut table = FutexTable::new();
        let registration = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake(key(1, 0x2000), 1), 0);
        assert_eq!(table.wake(key(1, 0x1000), 1), 1);

        block_on(registration.notify().notified());
        table.complete_wait(registration);
    }

    #[test]
    fn process_memory_identity_partitions_same_guest_address() {
        let mut table = FutexTable::new();
        let left = table.prepare_wait(key(1, 0x1000));
        let right = table.prepare_wait(key(2, 0x1000));

        assert_eq!(table.wake(key(1, 0x1000), 1), 1);
        assert_eq!(table.wake(key(2, 0x1000), 1), 1);

        block_on(left.notify().notified());
        block_on(right.notify().notified());
        table.complete_wait(left);
        table.complete_wait(right);
    }

    #[test]
    fn wake_all_reports_all_waiters_for_key() {
        let mut table = FutexTable::new();
        let first = table.prepare_wait(key(1, 0x1000));
        let second = table.prepare_wait(key(1, 0x1000));

        assert_eq!(table.wake_all(key(1, 0x1000)), 2);

        block_on(first.notify().notified());
        block_on(second.notify().notified());
        table.complete_wait(first);
        table.complete_wait(second);
    }
}
