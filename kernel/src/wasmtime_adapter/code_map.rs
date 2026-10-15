//! The wasm code the program service has loaded, by address.
//!
//! An instruction sample is a bare address; this map is what names the
//! wasm function behind one. Each loaded image registers its executable
//! text range and the program it was loaded for, and stays registered for
//! as long as the kernel object that owns the compiled code does: the
//! [`CodeRegistration`] that object holds removes the entry when it drops.
//!
//! The function table of an image is built only when a sample first lands
//! in it, from Wasmtime's own function map (`functions()`), so a kernel
//! that never profiles pays one list entry per loaded image and nothing per
//! function.
//!
//! The map's lock is taken by loads, drops and the sample drain, never by
//! an interrupt handler, so it does not mask interrupts.

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::{Mutex, Once};
use wasmtime::component::Component;
use wasmtime::{Module, ModuleFunction};

use crate::{CodeAttribution, SampledCode, WasmFunctionSample};

/// Every registered image, shared between the service that loads code
/// and the registrations that retire it.
#[derive(Clone)]
pub(crate) struct CodeMap {
    inner: Arc<CodeMapInner>,
}

struct CodeMapInner {
    next_id: AtomicU64,
    images: Mutex<Vec<CodeImage>>,
}

/// One loaded image.
struct CodeImage {
    id: u64,
    /// The executable text, at the address it runs at: the runtime
    /// publishes code in place, so this is where a sample lands.
    text: Range<usize>,
    module: Arc<str>,
    code: LoadedCode,
    /// The image's functions ordered by text offset, built on first use.
    functions: Once<Box<[FunctionSpan]>>,
}

/// The compiled artifact an image is the text of. Held so the function
/// table can be read when it is first needed; it does not extend the
/// code's life past its owner's, because the owner's registration drops
/// this entry.
enum LoadedCode {
    Component(Component),
    Module(Module),
}

struct FunctionSpan {
    offset: usize,
    len: usize,
    sample: WasmFunctionSample,
}

/// Keeps an image in the map; dropping it removes the image.
///
/// Held by the kernel object that owns the compiled code, next to it.
pub(crate) struct CodeRegistration {
    map: CodeMap,
    id: u64,
}

impl CodeMap {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(CodeMapInner {
                next_id: AtomicU64::new(0),
                images: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Registers a component loaded for `module`.
    pub(crate) fn register_component(
        &self,
        module: &str,
        component: &Component,
    ) -> CodeRegistration {
        self.register(
            module,
            component.text(),
            LoadedCode::Component(component.clone()),
        )
    }

    /// Registers a core module loaded for `module`.
    pub(crate) fn register_module(&self, module: &str, core_module: &Module) -> CodeRegistration {
        self.register(
            module,
            core_module.text(),
            LoadedCode::Module(core_module.clone()),
        )
    }

    fn register(&self, module: &str, text: &[u8], code: LoadedCode) -> CodeRegistration {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let text = text.as_ptr_range();
        let image = CodeImage {
            id,
            text: text.start as usize..text.end as usize,
            module: Arc::from(module),
            code,
            functions: Once::new(),
        };
        self.inner.images.lock().push(image);
        CodeRegistration {
            map: self.clone(),
            id,
        }
    }
}

impl Drop for CodeRegistration {
    fn drop(&mut self) {
        let removed = {
            let mut images = self.map.inner.images.lock();
            let position = images
                .iter()
                .position(|image| image.id == self.id)
                .unwrap_or_else(|| panic!("code image {} was unregistered twice", self.id));
            images.swap_remove(position)
        };
        // Dropped outside the lock: releasing the last reference to an
        // image unpublishes its code, which may wait on other processors.
        drop(removed);
    }
}

impl CodeImage {
    fn function_spans(&self) -> Box<[FunctionSpan]> {
        let mut spans = match &self.code {
            LoadedCode::Component(component) => self.spans(component.functions()),
            LoadedCode::Module(module) => self.spans(module.functions()),
        };
        spans.sort_by_key(|span| span.offset);
        spans.into_boxed_slice()
    }

    fn spans(&self, functions: impl Iterator<Item = ModuleFunction>) -> Vec<FunctionSpan> {
        functions
            .map(|function| FunctionSpan {
                offset: function.offset,
                len: function.len,
                sample: WasmFunctionSample {
                    module: self.module.clone(),
                    static_module: function.module.as_u32(),
                    index: function.index.as_u32(),
                    name: function.name.as_deref().map(Arc::from),
                },
            })
            .collect()
    }

    fn attribute(&self, address: usize) -> SampledCode {
        let offset = address - self.text.start;
        let functions = self.functions.call_once(|| self.function_spans());
        let after = functions.partition_point(|span| span.offset <= offset);
        match functions[..after].last() {
            Some(span) if offset - span.offset < span.len => SampledCode::Wasm(span.sample.clone()),
            _ => SampledCode::WasmTrampoline {
                module: self.module.clone(),
            },
        }
    }
}

impl CodeAttribution for CodeMap {
    fn attribute(&self, address: usize) -> SampledCode {
        let images = self.inner.images.lock();
        images
            .iter()
            .find(|image| image.text.contains(&address))
            .map_or(SampledCode::Native(address as u64), |image| {
                image.attribute(address)
            })
    }
}
