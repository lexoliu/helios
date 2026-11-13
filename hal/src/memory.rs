use core::ptr::NonNull;

use crate::numa::NodeId;

/// Usable memory the kernel may own, in the kernel's direct map, and the
/// node it sits on.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    span: NonNull<[u8]>,
    node: NodeId,
}

impl MemoryRegion {
    /// # Safety
    ///
    /// `span` is usable memory nothing else owns, and it outlives the
    /// kernel.
    pub const unsafe fn new(span: NonNull<[u8]>, node: NodeId) -> Self {
        Self { span, node }
    }

    pub fn start(&self) -> usize {
        self.span.as_ptr().cast::<u8>() as usize
    }

    pub fn end(&self) -> usize {
        self.start() + self.len()
    }

    pub fn len(&self) -> usize {
        self.span.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn node(&self) -> NodeId {
        self.node
    }
}
