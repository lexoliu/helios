extern crate alloc;

use alloc::sync::Arc;

use bytes::Bytes;
use lru::LruCache;

pub struct ComponentCache<Component> {
    budget_bytes: usize,
    resident_bytes: usize,
    entries: LruCache<Bytes, Arc<Component>>,
}

impl<Component> ComponentCache<Component> {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            entries: LruCache::unbounded(),
        }
    }

    pub fn get(&mut self, artifact: &[u8]) -> Option<Arc<Component>> {
        self.entries.get(artifact).cloned()
    }

    pub fn insert_if_missing(
        &mut self,
        artifact: Bytes,
        component: Arc<Component>,
    ) -> Arc<Component> {
        if let Some(existing) = self.entries.get(artifact.as_ref()).cloned() {
            return existing;
        }

        self.resident_bytes = self
            .resident_bytes
            .checked_add(artifact.len())
            .expect("component cache byte accounting overflow");
        let replaced = self.entries.put(artifact, component.clone());
        assert!(
            replaced.is_none(),
            "component cache replaced an entry after miss revalidation"
        );
        self.evict_to_budget();
        component
    }

    fn evict_to_budget(&mut self) {
        while self.resident_bytes > self.budget_bytes {
            let Some((artifact, _component)) = self.entries.pop_lru() else {
                panic!("component cache accounting lost track of resident bytes");
            };
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(artifact.len())
                .expect("component cache byte accounting underflow");
        }
    }
}
