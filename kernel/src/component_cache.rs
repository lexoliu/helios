extern crate alloc;

use alloc::sync::Arc;

use lru::LruCache;

pub struct ComponentCache<Component> {
    budget_bytes: usize,
    resident_bytes: usize,
    entries: LruCache<Arc<[u8]>, Arc<Component>>,
}

impl<Component> ComponentCache<Component> {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            budget_bytes,
            resident_bytes: 0,
            entries: LruCache::unbounded(),
        }
    }

    pub fn get(&mut self, wasm: &[u8]) -> Option<Arc<Component>> {
        self.entries.get(wasm).cloned()
    }

    pub fn insert_if_missing(
        &mut self,
        wasm: Arc<[u8]>,
        component: Arc<Component>,
    ) -> Arc<Component> {
        if let Some(existing) = self.entries.get(wasm.as_ref()).cloned() {
            return existing;
        }

        self.resident_bytes = self
            .resident_bytes
            .checked_add(wasm.len())
            .expect("component cache byte accounting overflow");
        let replaced = self.entries.put(wasm, component.clone());
        assert!(
            replaced.is_none(),
            "component cache replaced an entry after miss revalidation"
        );
        self.evict_to_budget();
        component
    }

    fn evict_to_budget(&mut self) {
        while self.resident_bytes > self.budget_bytes {
            let Some((wasm, _component)) = self.entries.pop_lru() else {
                panic!("component cache accounting lost track of resident bytes");
            };
            self.resident_bytes = self
                .resident_bytes
                .checked_sub(wasm.len())
                .expect("component cache byte accounting underflow");
        }
    }
}
