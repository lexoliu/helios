extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

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

/// Readiness of one registered source, shared by the registry and every
/// clone of its registration.
///
/// The flag is the level and the notification is the edge:
/// [`PollRegistry::mark_ready`] publishes the flag and then broadcasts.
/// A broadcast banks nothing, so a waiter arms on it *before* it reads
/// the flag; a `mark_ready` landing between the two completes the wait
/// it just missed instead of being lost.
struct PollState {
    ready: AtomicBool,
    notify: Notify,
}

#[derive(Clone)]
pub struct PollRegistration {
    key: PollKey,
    state: Arc<PollState>,
}

impl PollRegistration {
    pub fn key(&self) -> PollKey {
        self.key
    }

    /// Resolves once the source is ready, and immediately when it
    /// already is.
    pub async fn ready(&self) {
        loop {
            let notified = self.state.notify.notified();
            if self.state.ready.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
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
    state: Arc<PollState>,
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
        let state = Arc::new(PollState {
            ready: AtomicBool::new(false),
            notify: Notify::new(),
        });
        self.entries.insert(
            key,
            PollEntry {
                state: state.clone(),
            },
        );
        Ok(PollRegistration { key, state })
    }

    pub fn mark_ready(&mut self, key: PollKey) -> Result<(), PollRegistryError> {
        let entry = self.entry_mut(key)?;
        entry.state.ready.store(true, Ordering::Release);
        entry.state.notify.notify_all();
        Ok(())
    }

    pub fn clear_ready(&mut self, key: PollKey) -> Result<(), PollRegistryError> {
        self.entry_mut(key)?
            .state
            .ready
            .store(false, Ordering::Release);
        Ok(())
    }

    pub fn is_ready(&self, key: PollKey) -> Result<bool, PollRegistryError> {
        Ok(self.entry(key)?.state.ready.load(Ordering::Acquire))
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

    /// Readiness is a level, not a permit: a registration that is not
    /// ready parks, and one that is ready resolves however many times it
    /// is asked.
    #[test]
    fn registration_wait_parks_until_the_source_is_ready() {
        let mut registry = PollRegistry::new();
        let key = PollKey::new(PollSourceKind::SOCKET, 3);
        let registration = registry.register(key).unwrap();

        assert!(
            futures_lite::future::block_on(futures_lite::future::poll_once(registration.ready()))
                .is_none()
        );

        registry.mark_ready(key).unwrap();
        futures_lite::future::block_on(registration.ready());
        futures_lite::future::block_on(registration.clone().ready());

        registry.clear_ready(key).unwrap();
        assert!(
            futures_lite::future::block_on(futures_lite::future::poll_once(registration.ready()))
                .is_none()
        );
    }
}
