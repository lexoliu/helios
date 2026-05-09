extern crate alloc;

use alloc::sync::Arc;
use bytes::Bytes;
use lru::LruCache;

pub struct ComponentCache<Component> {
    budget_bytes: usize,
    resident_bytes: usize,
    entries: LruCache<Bytes, Arc<Component>>,
    identity_entries: LruCache<ArtifactIdentity, Arc<Component>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
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
            identity_entries: LruCache::unbounded(),
        }
    }

    pub fn get(&mut self, artifact: &Bytes) -> Option<Arc<Component>> {
        let identity = ArtifactIdentity::from_bytes(artifact);
        if let Some(component) = self.identity_entries.get(&identity) {
            return Some(component.clone());
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
        self.identity_entries.put(identity, component.clone());
        self.evict_to_budget();
        component
    }

    fn evict_to_budget(&mut self) {
        while self.resident_bytes > self.budget_bytes {
            let Some((artifact, _component)) = self.entries.pop_lru() else {
                panic!("component cache accounting lost track of resident bytes");
            };
            let identity = ArtifactIdentity::from_bytes(&artifact);
            let _ = self.identity_entries.pop(&identity);
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(artifact.len())
                .expect("component cache byte accounting underflow");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_lookup_uses_pointer_key_without_content_scan() {
        let mut cache = ComponentCache::new(1024);
        let artifact = Bytes::from_static(b"component");
        let component = Arc::new(7_u32);
        let inserted = cache.insert_if_missing(artifact.clone(), component);

        let hit = cache
            .get(&artifact)
            .expect("identity lookup should hit inserted artifact");
        assert!(Arc::ptr_eq(&inserted, &hit));
    }

    #[test]
    fn eviction_removes_identity_entry() {
        let mut cache = ComponentCache::new(4);
        let first_artifact = Bytes::from_static(b"aaaa");
        let second_artifact = Bytes::from_static(b"bbbb");
        let first_component = cache.insert_if_missing(first_artifact.clone(), Arc::new(1_u32));

        cache.insert_if_missing(second_artifact, Arc::new(2_u32));

        let replacement = cache.insert_if_missing(first_artifact, Arc::new(3_u32));
        assert!(!Arc::ptr_eq(&first_component, &replacement));
    }
}
