use core::ptr::NonNull;

pub type MemoryRegion = NonNull<[u8]>;

pub trait MemoryMap: Send + Sync + 'static {
    type Regions<'a>: Iterator<Item = MemoryRegion> + 'a
    where
        Self: 'a;

    fn regions(&self) -> Self::Regions<'_>;
}
