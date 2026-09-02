use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::ptr::NonNull;

use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::DmaTranslation;

pub trait DeviceBus: Send + Sync + 'static {
    type DmaPool: DmaPool;

    fn read_u8(&self, offset: usize) -> u8 {
        let word_offset = offset & !0x3;
        let byte_index = offset & 0x3;
        self.read_u32(word_offset).to_le_bytes()[byte_index]
    }

    fn read_u32(&self, offset: usize) -> u32;
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

#[derive(Clone, Copy)]
pub struct OffsetDmaPool {
    virtual_to_physical_offset: usize,
}

pub struct OffsetDmaBuffer {
    ptr: NonNull<u8>,
    layout: Layout,
    physical_address: u64,
}

impl OffsetDmaPool {
    pub const fn new(virtual_to_physical_offset: usize) -> Self {
        Self {
            virtual_to_physical_offset,
        }
    }

    fn translate(self, ptr: *const u8) -> IoResult<u64> {
        let virtual_address = ptr as usize;
        let physical_address = virtual_address
            .checked_sub(self.virtual_to_physical_offset)
            .ok_or(IoError::OutOfBounds)?;
        Ok(physical_address as u64)
    }
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

    fn addressing(&self) -> DmaAddressing {
        DmaAddressing::Physical
    }
}

impl DmaPool for OffsetDmaPool {
    type Buffer = OffsetDmaBuffer;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer> {
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(ptr).ok_or(IoError::DeviceFault)?;
        let physical_address = self.translate(ptr.as_ptr())?;
        Ok(OffsetDmaBuffer {
            ptr,
            layout,
            physical_address,
        })
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

impl DmaBuffer for OffsetDmaBuffer {
    fn phys_addr(&self) -> u64 {
        self.physical_address
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.layout.size()
    }
}

impl Drop for IdentityDmaBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

impl Drop for OffsetDmaBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

impl<P: DmaPool> DeviceBus for MmioBus<P> {
    type DmaPool = P;

    fn read_u8(&self, offset: usize) -> u8 {
        unsafe { self.checked_byte_ptr(offset).read_volatile() }
    }

    fn read_u32(&self, offset: usize) -> u32 {
        unsafe { self.checked_ptr(offset).read_volatile() }
    }

    fn write_u32(&self, offset: usize, value: u32) {
        unsafe {
            self.checked_ptr(offset).write_volatile(value);
        }
    }

    fn dma(&self) -> &Self::DmaPool {
        &self.dma
    }
}

unsafe impl Send for IdentityDmaBuffer {}
unsafe impl Sync for IdentityDmaBuffer {}
unsafe impl Send for OffsetDmaBuffer {}
unsafe impl Sync for OffsetDmaBuffer {}

unsafe impl<P: Send> Send for MmioBus<P> {}
unsafe impl<P: Sync> Sync for MmioBus<P> {}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use helios_hal::io::IoError;
    use helios_hal::iommu::{DmaTranslation, DmaWindow, IoVirtAddr};

    use super::{DmaAddressing, DmaBuffer, DmaPool, IdentityDmaPool, PlatformDmaPool};

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
