use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

/// Per-processor TLS slots required by Wasmtime's custom platform ABI.
///
/// Wasmtime reserves slot zero for trap/runtime activation state, slot one for
/// Component Model Async state, and slot two for the fiber currently executing
/// on this processor — which is what lets the page-fault trampoline find the
/// store it must block against. Keeping the slot dispatch in one typed owner
/// prevents backend C hooks from silently drifting when the ABI changes.
pub struct WasmtimeTlsSlots {
    runtime: AtomicPtr<u8>,
    component_async: AtomicPtr<u8>,
    current_fiber: AtomicPtr<u8>,
}

impl WasmtimeTlsSlots {
    /// Trap-handler and runtime activation state.
    pub const RUNTIME: usize = 0;
    /// Component Model Async task state.
    pub const COMPONENT_ASYNC: usize = 1;
    /// The fiber running on this processor, published for the duration
    /// of its resume. `wasmtime::block_on_current_fiber` reads it, which
    /// is how a page-fault trampoline blocks the fiber it interrupted.
    pub const CURRENT_FIBER: usize = 2;

    pub const fn new() -> Self {
        Self {
            runtime: AtomicPtr::new(ptr::null_mut()),
            component_async: AtomicPtr::new(ptr::null_mut()),
            current_fiber: AtomicPtr::new(ptr::null_mut()),
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
            Self::CURRENT_FIBER => &self.current_fiber,
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
    fn every_slot_is_independent() {
        let slots = WasmtimeTlsSlots::new();
        let mut runtime = 1u8;
        let mut component = 2u8;
        let mut fiber = 3u8;

        let runtime_ptr = &mut runtime as *mut u8;
        let component_ptr = &mut component as *mut u8;
        let fiber_ptr = &mut fiber as *mut u8;
        slots.set(WasmtimeTlsSlots::RUNTIME, runtime_ptr);
        slots.set(WasmtimeTlsSlots::COMPONENT_ASYNC, component_ptr);
        slots.set(WasmtimeTlsSlots::CURRENT_FIBER, fiber_ptr);

        assert_eq!(slots.get(WasmtimeTlsSlots::RUNTIME), runtime_ptr);
        assert_eq!(slots.get(WasmtimeTlsSlots::COMPONENT_ASYNC), component_ptr);
        assert_eq!(slots.get(WasmtimeTlsSlots::CURRENT_FIBER), fiber_ptr);
    }

    #[test]
    #[should_panic(expected = "unsupported TLS slot 3")]
    fn unknown_slot_panics() {
        WasmtimeTlsSlots::new().get(3);
    }
}
