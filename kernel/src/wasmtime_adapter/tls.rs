use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Per-processor TLS slots required by Wasmtime's custom platform ABI.
///
/// Wasmtime reserves slot zero for trap/runtime activation state and slot one
/// for Component Model Async state. Keeping the slot dispatch in one typed
/// owner prevents backend C hooks from silently drifting when the ABI changes.
pub struct WasmtimeTlsSlots {
    runtime: AtomicPtr<u8>,
    component_async: AtomicPtr<u8>,
}

impl WasmtimeTlsSlots {
    /// Trap-handler and runtime activation state.
    pub const RUNTIME: usize = 0;
    /// Component Model Async task state.
    pub const COMPONENT_ASYNC: usize = 1;

    pub const fn new() -> Self {
        Self {
            runtime: AtomicPtr::new(ptr::null_mut()),
            component_async: AtomicPtr::new(ptr::null_mut()),
        }
    }

    pub fn get(&self, slot: usize) -> *mut u8 {
        self.slot(slot).load(Ordering::Acquire)
    }

    pub fn set(&self, slot: usize, ptr: *mut u8) {
        self.slot(slot).store(ptr, Ordering::Release);
    }

    fn slot(&self, slot: usize) -> &AtomicPtr<u8> {
        match slot {
            Self::RUNTIME => &self.runtime,
            Self::COMPONENT_ASYNC => &self.component_async,
            _ => panic!("Wasmtime requested unsupported TLS slot {slot}"),
        }
    }
}

impl Default for WasmtimeTlsSlots {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::WasmtimeTlsSlots;

    #[test]
    fn runtime_and_component_slots_are_independent() {
        let slots = WasmtimeTlsSlots::new();
        let mut runtime = 1u8;
        let mut component = 2u8;

        let runtime_ptr = &mut runtime as *mut u8;
        let component_ptr = &mut component as *mut u8;
        slots.set(WasmtimeTlsSlots::RUNTIME, runtime_ptr);
        slots.set(WasmtimeTlsSlots::COMPONENT_ASYNC, component_ptr);

        assert_eq!(slots.get(WasmtimeTlsSlots::RUNTIME), runtime_ptr);
        assert_eq!(slots.get(WasmtimeTlsSlots::COMPONENT_ASYNC), component_ptr);
    }

    #[test]
    #[should_panic(expected = "unsupported TLS slot 2")]
    fn unknown_slot_panics() {
        WasmtimeTlsSlots::new().get(2);
    }
}
