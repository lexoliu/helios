extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use bytes::Bytes;
use lru::LruCache;

pub struct ComponentCache<Component> {
    budget_bytes: usize,
    resident_bytes: usize,
    entries: LruCache<Bytes, Arc<Component>>,
    identity_entries: Vec<ComponentCacheIdentityEntry<Component>>,
}

struct ComponentCacheIdentityEntry<Component> {
    identity: ArtifactIdentity,
    component: Arc<Component>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ArtifactIdentity {
    ptr: usize,
    len: usize,
}

impl ArtifactIdentity {
    fn from_bytes(bytes: &Bytes) -> Self {
        Self {
            ptr: bytes.as_ptr() as usize,
            len: bytes.len(),
        }
    }
}

impl<Component> ComponentCache<Component> {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            entries: LruCache::unbounded(),
            identity_entries: Vec::new(),
        }
    }

    pub fn get(&mut self, artifact: &Bytes) -> Option<Arc<Component>> {
        let identity = ArtifactIdentity::from_bytes(artifact);
        if let Some(entry) = self
            .identity_entries
            .iter()
            .find(|entry| entry.identity == identity)
        {
            return Some(entry.component.clone());
        }

        self.entries.get(artifact.as_ref()).cloned()
    }

    pub fn insert_if_missing(
        &mut self,
        artifact: Bytes,
        component: Arc<Component>,
    ) -> Arc<Component> {
        if let Some(existing) = self.get(&artifact) {
            return existing;
        }

        let identity = ArtifactIdentity::from_bytes(&artifact);
        self.resident_bytes = self
            .resident_bytes
            .checked_add(artifact.len())
            .expect("component cache byte accounting overflow");
        let replaced = self.entries.put(artifact, component.clone());
        assert!(
            replaced.is_none(),
            "component cache replaced an entry after miss revalidation"
        );
        self.identity_entries.push(ComponentCacheIdentityEntry {
            identity,
            component: component.clone(),
        });
        self.evict_to_budget();
        component
    }

    fn evict_to_budget(&mut self) {
        while self.resident_bytes > self.budget_bytes {
            let Some((artifact, _component)) = self.entries.pop_lru() else {
                panic!("component cache accounting lost track of resident bytes");
            };
            let identity = ArtifactIdentity::from_bytes(&artifact);
            self.identity_entries
                .retain(|entry| entry.identity != identity);
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(artifact.len())
                .expect("component cache byte accounting underflow");
        }
    }
}
