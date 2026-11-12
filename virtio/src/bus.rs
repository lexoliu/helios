use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::marker::PhantomData;
use core::ptr::NonNull;

use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::DmaTranslation;
use helios_hal::mmio;
use helios_hal::tagging::{AllocationTags, NoAllocationTags, Tag, TagSet, TaggedView};

pub trait DeviceBus: Send + Sync + 'static {
    type DmaPool: DmaPool;

    fn read_u8(&self, offset: usize) -> u8 {
        let word_offset = offset & !0x3;
        let byte_index = offset & 0x3;
        self.read_u32(word_offset).to_le_bytes()[byte_index]
    }

    /// Reads one naturally aligned 16-bit register.
    ///
    /// Not derived from [`DeviceBus::read_u32`] on purpose: a device
    /// bounds configuration accesses by the length of the structure it
    /// exposes, and a dword read that straddles that end (virtio-net's
    /// `max_virtqueue_pairs` when MQ is the last feature it offers)
    /// comes back all-ones rather than as the field.
    fn read_u16(&self, offset: usize) -> u16;
    fn read_u32(&self, offset: usize) -> u32;

    /// Writes one 8-bit register.
    ///
    /// Not derived from [`DeviceBus::write_u32`] either: a
    /// read-modify-write of the surrounding word would write the three
    /// neighbouring bytes as well, and in a device configuration
    /// register file those belong to the device.
    fn write_u8(&self, offset: usize, value: u8);
    fn write_u32(&self, offset: usize, value: u32);
    fn dma(&self) -> &Self::DmaPool;
}

/// What kind of address a [`DmaPool`] hands to a device.
///
/// This is what decides whether a driver has to negotiate
/// VIRTIO_F_ACCESS_PLATFORM: a device that is given platform addresses
/// must be told so, or it would read the descriptor rings as physical
/// addresses and translate nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaAddressing {
    /// The device sees physical addresses.
    Physical,
    /// The device sees addresses the platform translates on its behalf.
    Platform,
}

pub trait DmaPool: Send + Sync + 'static {
    type Buffer: DmaBuffer;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer>;
    fn dma_addr(&self, ptr: *const u8) -> IoResult<u64>;

    /// The address a device has to issue to reach `bytes` of physical
    /// memory at `physical`, which this pool did not allocate.
    ///
    /// [`DmaPool::dma_addr`] answers for a buffer the driver holds a
    /// pointer to, which is a buffer in whatever mapping the driver
    /// allocates out of. This one answers for memory that is somebody
    /// else's and is named by its physical address alone — a guest's
    /// own pinned pages, handed to the device by address instead of
    /// copied — and it is the only way such a run reaches a descriptor.
    /// A pool whose device sees physical addresses answers with the
    /// address it was given; one behind a translation answers with the
    /// address inside the device's domain, and refuses a run the domain
    /// does not map contiguously rather than handing over one the
    /// device would read the wrong half of.
    fn device_range(&self, physical: u64, bytes: u64) -> IoResult<u64>;

    /// What kind of address this pool hands out.
    fn addressing(&self) -> DmaAddressing;
}

pub trait DmaBuffer: Send + Sync + 'static {
    fn phys_addr(&self) -> u64;
    fn as_ptr(&self) -> *mut u8;
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Copy, Default)]
pub struct IdentityDmaPool;

pub struct IdentityDmaBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
}

/// A pool over the kernel's direct map. On a processor that checks memory
/// tags (`A::checking()`), each allocation is reached through the tagged
/// view, carries a tag of its own and is bordered by two redzones, so a
/// linear overrun in either direction lands on a granule no live tag
/// matches. Otherwise the allocation is exactly the layout asked for.
///
/// Concurrency contract: the pool is an immutable value, built once while
/// the device is brought up, and every operation is lock-free. Allocation
/// and release run at queue bring-up and teardown, each owning the
/// granules it tags; the submission path only translates addresses.
pub struct OffsetDmaPool<A = NoAllocationTags> {
    view: TaggedView,
    tags: PhantomData<fn() -> A>,
}

/// One ring from an [`OffsetDmaPool`]: the object, reached through the
/// tagged view, inside an allocation that also holds its redzones.
pub struct OffsetDmaBuffer<A: AllocationTags = NoAllocationTags> {
    /// The tagged-view pointer carrying the object's tag.
    ptr: NonNull<u8>,
    /// The object's range in the tagged view, without its logical tag.
    object: NonNull<[u8]>,
    /// The allocation that holds the object and its redzones, in the
    /// direct map, and the layout it was allocated with.
    allocation: NonNull<u8>,
    allocation_layout: Layout,
    /// Bytes of each redzone, zero when the pool allocated none.
    redzone: usize,
    physical_address: u64,
    tags: PhantomData<fn() -> A>,
}

impl<A> Clone for OffsetDmaPool<A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A> Copy for OffsetDmaPool<A> {}

impl OffsetDmaPool<NoAllocationTags> {
    /// A pool over a direct map at `virtual_to_physical_offset`, on
    /// hardware that stores no allocation tags.
    pub const fn new(virtual_to_physical_offset: usize) -> Self {
        Self {
            view: TaggedView::direct_map(virtual_to_physical_offset),
            tags: PhantomData,
        }
    }
}

impl<A: AllocationTags> OffsetDmaPool<A> {
    /// A pool whose rings are reached through `view`.
    ///
    /// # Safety
    ///
    /// When `A::checking()` is true, `view` is tag-capable for every
    /// physical address the heap hands out, with `line_bytes` and
    /// `A::GRANULE_BYTES` powers of two, so that the ranges the pool gives
    /// `A::assign` and `A::store` meet the contract of those functions.
    pub const unsafe fn tagged(view: TaggedView) -> Self {
        Self {
            view,
            tags: PhantomData,
        }
    }

    fn translate(self, ptr: *const u8) -> IoResult<u64> {
        self.view
            .physical(A::strip(ptr as usize))
            .ok_or(IoError::OutOfBounds)
    }
}

/// The range of `len` bytes at `addr`.
fn range_at(addr: usize, len: usize) -> NonNull<[u8]> {
    let start = NonNull::new(addr as *mut u8).expect("a mapped range does not start at null");
    NonNull::slice_from_raw_parts(start, len)
}

#[derive(Clone)]
pub struct MmioBus<P = IdentityDmaPool> {
    base: NonNull<u8>,
    size: usize,
    dma: P,
}

impl<P> MmioBus<P> {
    /// # Safety
    ///
    /// `base..base+size` must name a valid, permanently mapped MMIO aperture
    /// for the lifetime of the returned bus, and all accesses through it must
    /// obey the device's register layout and aliasing requirements.
    pub unsafe fn new(base: NonNull<u8>, size: usize, dma: P) -> IoResult<Self> {
        if size < 0x100 {
            return Err(IoError::Unsupported);
        }

        Ok(Self { base, size, dma })
    }

    fn checked_ptr(&self, offset: usize) -> *mut u32 {
        let end = offset
            .checked_add(core::mem::size_of::<u32>())
            .unwrap_or_else(|| panic!("MMIO offset overflow"));
        assert!(end <= self.size, "MMIO access out of range");
        assert!(
            offset.is_multiple_of(core::mem::align_of::<u32>()),
            "MMIO access misaligned"
        );

        unsafe { self.base.as_ptr().add(offset).cast::<u32>() }
    }

    fn checked_byte_ptr(&self, offset: usize) -> *mut u8 {
        assert!(offset < self.size, "MMIO byte access out of range");

        unsafe { self.base.as_ptr().add(offset) }
    }

    fn checked_half_ptr(&self, offset: usize) -> *mut u16 {
        let end = offset
            .checked_add(core::mem::size_of::<u16>())
            .unwrap_or_else(|| panic!("MMIO offset overflow"));
        assert!(end <= self.size, "MMIO half-word access out of range");
        assert!(
            offset.is_multiple_of(core::mem::align_of::<u16>()),
            "MMIO half-word access misaligned"
        );

        unsafe { self.base.as_ptr().add(offset).cast::<u16>() }
    }
}

impl DmaPool for IdentityDmaPool {
    type Buffer = IdentityDmaBuffer;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer> {
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(IoError::DeviceFault)?;
        Ok(IdentityDmaBuffer { ptr, layout })
    }

    fn dma_addr(&self, ptr: *const u8) -> IoResult<u64> {
        Ok(ptr as usize as u64)
    }

    /// Physical addresses and host addresses are the same thing in this
    /// pool, so a run of somebody else's memory is reached where it is.
    fn device_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    fn addressing(&self) -> DmaAddressing {
        DmaAddressing::Physical
    }
}

impl<A: AllocationTags> DmaPool for OffsetDmaPool<A> {
    type Buffer = OffsetDmaBuffer<A>;

    /// With checking on, the allocation is `[redzone][object][redzone]`.
    /// The unit is the larger of a tag granule and the view's line, so
    /// neither a granule nor a cache-writeback line is shared with a heap
    /// neighbour reached through the direct map; the object is `layout`
    /// aligned and padded to it, and each redzone is one unit.
    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer> {
        let (object_layout, redzone) = if A::checking() {
            let unit = A::GRANULE_BYTES.max(self.view.line_bytes());
            let object = layout
                .align_to(unit)
                .map_err(|_| IoError::OutOfBounds)?
                .pad_to_align();
            (object, unit)
        } else {
            (layout, 0)
        };
        let allocation_size = object_layout
            .size()
            .checked_add(2 * redzone)
            .ok_or(IoError::OutOfBounds)?;
        let allocation_layout = Layout::from_size_align(allocation_size, object_layout.align())
            .map_err(|_| IoError::OutOfBounds)?;

        let allocation = unsafe { alloc_zeroed(allocation_layout) };
        let allocation = NonNull::new(allocation).ok_or(IoError::DeviceFault)?;
        let direct_object = allocation.as_ptr() as usize + redzone;
        let Some(physical_address) = self.view.physical(direct_object) else {
            // SAFETY: allocated above with this layout and not yet shared.
            unsafe { dealloc(allocation.as_ptr(), allocation_layout) };
            return Err(IoError::OutOfBounds);
        };

        let view_object = self.view.tagged(direct_object);
        let len = object_layout.size();
        let tag = if redzone == 0 {
            Tag::UNCHECKED
        } else {
            // SAFETY: the allocation is this call's alone, and the view
            // maps it tag-capable (`tagged`'s contract). The redzones and
            // the object are `unit`-aligned multiples of `unit`.
            unsafe {
                A::store(range_at(view_object - redzone, redzone), Tag::FREE);
                A::store(range_at(view_object + len, redzone), Tag::FREE);
                A::assign(range_at(view_object, len), TagSet::EMPTY)
            }
        };
        let ptr = NonNull::new(A::apply(view_object, tag) as *mut u8)
            .expect("a tagged ring address is not null");
        Ok(OffsetDmaBuffer {
            ptr,
            object: range_at(view_object, len),
            allocation,
            allocation_layout,
            redzone,
            physical_address,
            tags: PhantomData,
        })
    }

    /// The device sees physical addresses, so a run named by one is
    /// already the address it has to issue. The offset this pool holds
    /// turns a kernel pointer into a physical address and has nothing
    /// to say about memory that arrived as one.
    fn device_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    fn dma_addr(&self, ptr: *const u8) -> IoResult<u64> {
        self.translate(ptr)
    }

    fn addressing(&self) -> DmaAddressing {
        DmaAddressing::Physical
    }
}

/// A DMA pool whose device-visible addresses go through the platform's
/// DMA translation.
///
/// Allocation and physical translation stay with the wrapped pool; this
/// one only turns a physical address into the address the device has to
/// issue for it. On a machine whose devices are confined by an IOMMU
/// that is an I/O virtual address inside the device's own domain, so a
/// device driven through this pool can reach nothing but the ranges the
/// domain maps — including nothing that belongs to another device.
///
/// Concurrency contract: the translation is built once, while the
/// device is brought up on the bootstrap processor, and is immutable
/// afterwards, so every submission path reads it without a lock.
pub struct PlatformDmaPool<P> {
    inner: P,
    translation: DmaTranslation,
}

/// A buffer from a [`PlatformDmaPool`], which reports the address the
/// device has to issue rather than its physical address.
pub struct PlatformDmaBuffer<B> {
    inner: B,
    device_address: u64,
}

impl<P> PlatformDmaPool<P> {
    pub const fn new(inner: P, translation: DmaTranslation) -> Self {
        Self { inner, translation }
    }

    /// The translation every address this pool hands out goes through.
    pub const fn translation(&self) -> &DmaTranslation {
        &self.translation
    }
}

impl<P: Clone> Clone for PlatformDmaPool<P> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            translation: self.translation,
        }
    }
}

impl<P: Copy> Copy for PlatformDmaPool<P> {}

impl<P: DmaPool> DmaPool for PlatformDmaPool<P> {
    type Buffer = PlatformDmaBuffer<P::Buffer>;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer> {
        let inner = self.inner.allocate_zeroed(layout)?;
        let bytes = u64::try_from(inner.len()).map_err(|_| IoError::OutOfBounds)?;
        let device_address = self.translation.device_range(inner.phys_addr(), bytes)?;
        Ok(PlatformDmaBuffer {
            inner,
            device_address,
        })
    }

    fn dma_addr(&self, ptr: *const u8) -> IoResult<u64> {
        let physical = self.inner.dma_addr(ptr)?;
        Ok(self.translation.device_address(physical)?)
    }

    fn device_range(&self, physical: u64, bytes: u64) -> IoResult<u64> {
        Ok(self.translation.device_range(physical, bytes)?)
    }

    fn addressing(&self) -> DmaAddressing {
        if self.translation.is_direct() {
            DmaAddressing::Physical
        } else {
            DmaAddressing::Platform
        }
    }
}

impl<B: DmaBuffer> DmaBuffer for PlatformDmaBuffer<B> {
    fn phys_addr(&self) -> u64 {
        self.device_address
    }

    fn as_ptr(&self) -> *mut u8 {
        self.inner.as_ptr()
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

impl DmaBuffer for IdentityDmaBuffer {
    fn phys_addr(&self) -> u64 {
        self.ptr.as_ptr() as usize as u64
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.layout.size()
    }
}

impl<A: AllocationTags> DmaBuffer for OffsetDmaBuffer<A> {
    fn phys_addr(&self) -> u64 {
        self.physical_address
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.object.len()
    }
}

impl Drop for IdentityDmaBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

impl<A: AllocationTags> Drop for OffsetDmaBuffer<A> {
    /// Returns the object to the free tag, so a stale ring pointer no longer
    /// matches, then frees the whole allocation through its direct-map
    /// pointer: the allocator writes its own metadata through the pointer
    /// it is given.
    fn drop(&mut self) {
        if self.redzone != 0 {
            // SAFETY: the object is this buffer's alone and was tagged
            // through the same view at allocation.
            unsafe { A::store(self.object, Tag::FREE) };
        }
        // SAFETY: allocated in `allocate_zeroed` with this layout.
        unsafe { dealloc(self.allocation.as_ptr(), self.allocation_layout) };
    }
}

impl<P: DmaPool> DeviceBus for MmioBus<P> {
    type DmaPool = P;

    fn read_u8(&self, offset: usize) -> u8 {
        unsafe { mmio::read_u8(self.checked_byte_ptr(offset)) }
    }

    fn read_u16(&self, offset: usize) -> u16 {
        unsafe { mmio::read_u16(self.checked_half_ptr(offset)) }
    }

    fn read_u32(&self, offset: usize) -> u32 {
        unsafe { mmio::read_u32(self.checked_ptr(offset)) }
    }

    fn write_u8(&self, offset: usize, value: u8) {
        unsafe {
            mmio::write_u8(self.checked_byte_ptr(offset), value);
        }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        unsafe {
            mmio::write_u32(self.checked_ptr(offset), value);
        }
    }

    fn dma(&self) -> &Self::DmaPool {
        &self.dma
    }
}

unsafe impl Send for IdentityDmaBuffer {}
unsafe impl Sync for IdentityDmaBuffer {}
unsafe impl<A: AllocationTags> Send for OffsetDmaBuffer<A> {}
unsafe impl<A: AllocationTags> Sync for OffsetDmaBuffer<A> {}

unsafe impl<P: Send> Send for MmioBus<P> {}
unsafe impl<P: Sync> Sync for MmioBus<P> {}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use helios_hal::io::IoError;
    use helios_hal::iommu::{DmaTranslation, DmaWindow, IoVirtAddr};
    use helios_hal::tagging::{AllocationTags, Tag, TagSet, TaggedView};

    use super::{
        DmaAddressing, DmaBuffer, DmaPool, IdentityDmaPool, OffsetDmaPool, PlatformDmaPool,
    };
    use crate::testing::{RecordingTags, TagOp};

    /// A direct map whose heap pointers are well above its offset, and a
    /// tagged view beyond every host heap address, so the two views never
    /// translate the same canonical address. The test pools never
    /// dereference a pointer in the tagged view.
    const DIRECT_OFFSET: usize = 0x10_0000;
    const TAGGED_OFFSET: usize = 1 << 47;
    const SPAN: usize = 1 << 47;
    const LINE: usize = 64;
    /// The redzone unit: the larger of `RecordingTags`' granule and `LINE`.
    const UNIT: usize = 64;

    fn view() -> TaggedView {
        TaggedView::new(DIRECT_OFFSET, TAGGED_OFFSET, SPAN, LINE)
    }

    fn tagged_pool<const CHECKING: bool>() -> OffsetDmaPool<RecordingTags<CHECKING>> {
        // SAFETY: `RecordingTags` touches no memory.
        unsafe { OffsetDmaPool::tagged(view()) }
    }

    /// A 6-byte, 2-aligned driver area: smaller and less aligned than a
    /// granule or a line.
    fn ring_layout() -> Layout {
        Layout::from_size_align(6, 2).expect("layout")
    }

    fn object_address(buffer: &impl DmaBuffer) -> usize {
        RecordingTags::<true>::strip(buffer.as_ptr() as usize)
    }

    #[test]
    fn tagged_ring_is_padded_to_granule_and_line() {
        let pool = tagged_pool::<true>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");
        let object = object_address(&buffer);

        assert_eq!(buffer.len(), LINE);
        assert_eq!(object % LINE, 0);
        assert_eq!((buffer.phys_addr() as usize + DIRECT_OFFSET) % LINE, 0);
        assert!(RecordingTags::<true>::take().contains(&TagOp::Assign {
            addr: object,
            len: LINE,
            avoid: TagSet::EMPTY,
        }));
    }

    #[test]
    fn tagged_ring_pointer_is_in_the_view_and_carries_its_tag() {
        let pool = tagged_pool::<true>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");
        let pointer = buffer.as_ptr() as usize;

        assert_eq!(
            object_address(&buffer),
            buffer.phys_addr() as usize + TAGGED_OFFSET
        );
        assert_eq!(pointer >> (usize::BITS - 8), 0x5);
        assert_eq!(
            Tag::new((pointer >> (usize::BITS - 8)) as u8),
            RecordingTags::<true>::ASSIGNED
        );
    }

    #[test]
    fn ring_physical_address_comes_from_the_direct_map() {
        let pool = tagged_pool::<true>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");
        let direct = buffer.phys_addr() as usize + DIRECT_OFFSET;

        // The direct-map address of the object, and the redzone on each
        // side of it, is the zeroed heap allocation itself.
        // SAFETY: the pool allocated `UNIT + LINE + UNIT` bytes there.
        let allocation = unsafe { core::slice::from_raw_parts((direct - UNIT) as *const u8, 192) };
        assert!(allocation.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn dma_addr_strips_and_accepts_both_views() {
        let pool = tagged_pool::<true>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");
        let physical = buffer.phys_addr();
        let direct = (physical as usize + DIRECT_OFFSET) as *const u8;

        assert_eq!(pool.dma_addr(buffer.as_ptr()), Ok(physical));
        assert_eq!(
            pool.dma_addr(buffer.as_ptr().wrapping_add(0x10)),
            Ok(physical + 0x10)
        );
        assert_eq!(pool.dma_addr(direct), Ok(physical));
        assert_eq!(pool.dma_addr(0x10 as *const u8), Err(IoError::OutOfBounds));
    }

    #[test]
    fn a_dropped_ring_goes_back_to_the_free_tag() {
        let pool = tagged_pool::<true>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");
        let object = object_address(&buffer);
        RecordingTags::<true>::take();

        drop(buffer);

        assert_eq!(
            RecordingTags::<true>::take(),
            [TagOp::Store {
                addr: object,
                len: LINE,
                tag: Tag::FREE,
            }]
        );
    }

    #[test]
    fn a_tagged_ring_is_bordered_by_free_tagged_redzones() {
        let pool = tagged_pool::<true>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");
        let object = object_address(&buffer);
        let free = |addr| TagOp::Store {
            addr,
            len: UNIT,
            tag: Tag::FREE,
        };

        let ops = RecordingTags::<true>::take();
        assert_eq!(ops.len(), 3, "two redzones and the object: {ops:?}");
        assert!(ops.contains(&free(object - UNIT)), "{ops:?}");
        assert!(ops.contains(&free(object + buffer.len())), "{ops:?}");
        assert!(
            ops.contains(&TagOp::Assign {
                addr: object,
                len: buffer.len(),
                avoid: TagSet::EMPTY,
            }),
            "{ops:?}"
        );
    }

    #[test]
    fn an_unchecked_pool_allocates_no_redzones() {
        let pool = tagged_pool::<false>();
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");

        assert_eq!(buffer.len(), ring_layout().size());
        assert_eq!(buffer.as_ptr() as usize >> (usize::BITS - 8), 0xf);
        assert_eq!(RecordingTags::<false>::take(), []);
        drop(buffer);
        assert_eq!(RecordingTags::<false>::take(), []);
    }

    #[test]
    fn an_untagged_pool_keeps_the_layout_and_the_direct_pointer() {
        let pool = OffsetDmaPool::new(0x10);
        let buffer = pool.allocate_zeroed(ring_layout()).expect("allocation");

        assert_eq!(buffer.len(), ring_layout().size());
        assert_eq!(buffer.phys_addr(), buffer.as_ptr() as usize as u64 - 0x10);
        assert_eq!(pool.dma_addr(buffer.as_ptr()), Ok(buffer.phys_addr()));
    }

    /// A platform with no translation unit hands the device physical
    /// addresses and must not ask it to negotiate anything.
    #[test]
    fn a_direct_platform_pool_hands_out_physical_addresses() {
        let pool = PlatformDmaPool::new(IdentityDmaPool, DmaTranslation::direct());
        let layout = Layout::from_size_align(256, 16).expect("layout");
        let buffer = pool.allocate_zeroed(layout).expect("allocation succeeds");

        assert_eq!(pool.addressing(), DmaAddressing::Physical);
        assert_eq!(buffer.phys_addr(), buffer.as_ptr() as usize as u64);
        assert_eq!(
            pool.dma_addr(buffer.as_ptr()).expect("translation"),
            buffer.as_ptr() as usize as u64
        );
    }

    #[test]
    fn a_confined_pool_reports_the_address_inside_the_domain() {
        // The window is built around a real allocation so the test can
        // name a physical address the pool will actually be asked for.
        let layout = Layout::from_size_align(4096, 4096).expect("layout");
        let reference = IdentityDmaPool
            .allocate_zeroed(layout)
            .expect("allocation succeeds");
        let physical = reference.phys_addr();
        let translation = DmaTranslation::confined()
            .with_window(DmaWindow {
                physical_start: physical,
                bytes: 4096,
                iova_start: IoVirtAddr::new(0x10_0000_0000),
            })
            .expect("the window fits");
        let pool = PlatformDmaPool::new(IdentityDmaPool, translation);

        assert_eq!(pool.addressing(), DmaAddressing::Platform);
        assert_eq!(
            pool.dma_addr(reference.as_ptr()).expect("translation"),
            0x10_0000_0000
        );
        assert_eq!(
            pool.dma_addr(unsafe { reference.as_ptr().add(0x40) })
                .expect("translation"),
            0x10_0000_0040
        );
    }

    /// A buffer the domain does not map would fault inside the device.
    /// Refusing it here names the buffer instead.
    #[test]
    fn a_buffer_outside_every_window_is_refused() {
        let translation = DmaTranslation::confined()
            .with_window(DmaWindow {
                physical_start: 0x4000_0000,
                bytes: 0x1000,
                iova_start: IoVirtAddr::new(0x10_0000_0000),
            })
            .expect("the window fits");
        let pool = PlatformDmaPool::new(IdentityDmaPool, translation);
        let layout = Layout::from_size_align(64, 8).expect("layout");

        assert_eq!(
            pool.allocate_zeroed(layout).err(),
            Some(IoError::OutOfBounds)
        );
    }
}
