extern crate alloc;

use hashbrown::HashMap;
use thiserror::Error;
use triomphe::Arc;

use crate::Notify;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PollSourceKind(u8);

impl PollSourceKind {
    pub const TIMER: Self = Self(1);
    pub const STREAM: Self = Self(2);
    pub const SOCKET: Self = Self(3);
    pub const FUTEX: Self = Self(4);
    pub const EVENT: Self = Self(5);
    pub const PROCESS_JOIN: Self = Self(6);

    pub const fn raw(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PollKey {
    kind: PollSourceKind,
    id: u64,
}

impl PollKey {
    pub const fn new(kind: PollSourceKind, id: u64) -> Self {
        Self { kind, id }
    }

    pub const fn kind(self) -> PollSourceKind {
        self.kind
    }

    pub const fn id(self) -> u64 {
        self.id
    }
}

#[derive(Clone)]
pub struct PollRegistration {
    key: PollKey,
    notify: Arc<Notify>,
}

impl PollRegistration {
    pub fn key(&self) -> PollKey {
        self.key
    }

    pub async fn ready(&self) {
        self.notify.notified().await;
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PollRegistryError {
    #[error("poll key is already registered: kind={kind:?} id={id}")]
    AlreadyRegistered { kind: PollSourceKind, id: u64 },
    #[error("poll key is not registered: kind={kind:?} id={id}")]
    NotRegistered { kind: PollSourceKind, id: u64 },
}

struct PollEntry {
    ready: bool,
    notify: Arc<Notify>,
}

#[derive(Default)]
pub struct PollRegistry {
    entries: HashMap<PollKey, PollEntry>,
}

impl PollRegistry {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn register(&mut self, key: PollKey) -> Result<PollRegistration, PollRegistryError> {
        if self.entries.contains_key(&key) {
            return Err(PollRegistryError::AlreadyRegistered {
                kind: key.kind(),
                id: key.id(),
            });
        }
        let notify = Arc::new(Notify::new());
        self.entries.insert(
            key,
            PollEntry {
                ready: false,
                notify: notify.clone(),
            },
        );
        Ok(PollRegistration { key, notify })
    }

    pub fn mark_ready(&mut self, key: PollKey) -> Result<(), PollRegistryError> {
        let entry = self.entry_mut(key)?;
        entry.ready = true;
        entry.notify.notify_all();
        Ok(())
    }

    pub fn clear_ready(&mut self, key: PollKey) -> Result<(), PollRegistryError> {
        self.entry_mut(key)?.ready = false;
        Ok(())
    }

    pub fn is_ready(&self, key: PollKey) -> Result<bool, PollRegistryError> {
        Ok(self.entry(key)?.ready)
    }

    pub fn remove(&mut self, key: PollKey) -> Result<(), PollRegistryError> {
        self.entries
            .remove(&key)
            .map(|_| ())
            .ok_or(PollRegistryError::NotRegistered {
                kind: key.kind(),
                id: key.id(),
            })
    }

    fn entry(&self, key: PollKey) -> Result<&PollEntry, PollRegistryError> {
        self.entries
            .get(&key)
            .ok_or(PollRegistryError::NotRegistered {
                kind: key.kind(),
                id: key.id(),
            })
    }

    fn entry_mut(&mut self, key: PollKey) -> Result<&mut PollEntry, PollRegistryError> {
        self.entries
            .get_mut(&key)
            .ok_or(PollRegistryError::NotRegistered {
                kind: key.kind(),
                id: key.id(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_tracks_readiness_by_typed_key() {
        let mut registry = PollRegistry::new();
        let key = PollKey::new(PollSourceKind::TIMER, 42);
        registry.register(key).unwrap();

        assert_eq!(registry.is_ready(key), Ok(false));
        registry.mark_ready(key).unwrap();
        assert_eq!(registry.is_ready(key), Ok(true));
        registry.clear_ready(key).unwrap();
        assert_eq!(registry.is_ready(key), Ok(false));
    }

    #[test]
    fn registry_rejects_duplicate_keys() {
        let mut registry = PollRegistry::new();
        let key = PollKey::new(PollSourceKind::SOCKET, 7);
        registry.register(key).unwrap();

        assert_eq!(
            registry.register(key).err(),
            Some(PollRegistryError::AlreadyRegistered {
                kind: PollSourceKind::SOCKET,
                id: 7
            })
        );
    }

    #[test]
    fn registration_wait_observes_mark_ready() {
        futures_lite::future::block_on(async {
            let mut registry = PollRegistry::new();
            let key = PollKey::new(PollSourceKind::EVENT, 1);
            let registration = registry.register(key).unwrap();
            registry.mark_ready(key).unwrap();
            registration.ready().await;
        });
    }
}
