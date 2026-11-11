use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::ptr::NonNull;

use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::DmaTranslation;
use helios_hal::mmio;

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

/// A range of memory a device cannot reach.
///
/// Names the buffer that was refused, so a driver bug that submits memory
/// outside the device's windows is reported with the address instead of as
/// a generic device fault.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{bytes} bytes at {address:#x} are not reachable by the device")]
pub struct DmaUnreachable {
    /// The host address of the first byte of the refused buffer, or the
    /// physical address of a run named by one.
    pub address: usize,
    pub bytes: usize,
}

impl From<DmaUnreachable> for IoError {
    fn from(_: DmaUnreachable) -> Self {
        Self::DeviceFault
    }
}

pub trait DmaPool: Send + Sync + 'static {
    type Buffer: DmaBuffer;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer>;

    /// The address a device has to issue to reach `buffer`.
    ///
    /// The whole slice has to be device-reachable and contiguous in the
    /// device's address space: a buffer that starts inside what the device
    /// can reach and runs past its end is refused, not translated by its
    /// first byte.
    fn dma_range(&self, buffer: &[u8]) -> Result<u64, DmaUnreachable>;

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

    fn dma_range(&self, buffer: &[u8]) -> Result<u64, DmaUnreachable> {
        Ok(buffer.as_ptr() as usize as u64)
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

    /// The device sees physical addresses, so a run named by one is
    /// already the address it has to issue. The offset this pool holds
    /// turns a kernel pointer into a physical address and has nothing
    /// to say about memory that arrived as one.
    fn device_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    /// The offset is linear, so a buffer is contiguous in physical memory
    /// exactly when it is in the kernel's address space.
    fn dma_range(&self, buffer: &[u8]) -> Result<u64, DmaUnreachable> {
        self.translate(buffer.as_ptr()).map_err(|_| DmaUnreachable {
            address: buffer.as_ptr() as usize,
            bytes: buffer.len(),
        })
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

    /// The whole slice has to sit inside one window of the translation.
    fn dma_range(&self, buffer: &[u8]) -> Result<u64, DmaUnreachable> {
        let unreachable = DmaUnreachable {
            address: buffer.as_ptr() as usize,
            bytes: buffer.len(),
        };
        let physical = self.inner.dma_range(buffer)?;
        let bytes = u64::try_from(buffer.len()).map_err(|_| unreachable)?;
        self.translation
            .device_range(physical, bytes)
            .map_err(|_| unreachable)
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
unsafe impl Send for OffsetDmaBuffer {}
unsafe impl Sync for OffsetDmaBuffer {}

unsafe impl<P: Send> Send for MmioBus<P> {}
unsafe impl<P: Sync> Sync for MmioBus<P> {}

#[cfg(test)]
mod tests {
    use core::alloc::Layout;

    use helios_hal::io::IoError;
    use helios_hal::iommu::{DmaTranslation, DmaWindow, IoVirtAddr};

    use super::{
        DmaAddressing, DmaBuffer, DmaPool, DmaUnreachable, IdentityDmaPool, PlatformDmaPool,
    };

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
            pool.dma_range(buffer_bytes(&buffer, 0, 256))
                .expect("translation"),
            buffer.as_ptr() as usize as u64
        );
    }

    /// The bytes `offset..offset + len` of `buffer`.
    fn buffer_bytes<B: DmaBuffer>(buffer: &B, offset: usize, len: usize) -> &[u8] {
        assert!(offset + len <= buffer.len());
        unsafe { core::slice::from_raw_parts(buffer.as_ptr().add(offset), len) }
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
            pool.dma_range(buffer_bytes(&reference, 0, 16))
                .expect("translation"),
            0x10_0000_0000
        );
        assert_eq!(
            pool.dma_range(buffer_bytes(&reference, 0x40, 16))
                .expect("translation"),
            0x10_0000_0040
        );
        assert_eq!(
            pool.dma_range(buffer_bytes(&reference, 0, 4096))
                .expect("a slice that fills the window translates"),
            0x10_0000_0000
        );
    }

    /// A slice that starts inside a window and runs past its end used to
    /// translate by its first byte. It is refused, with the address.
    #[test]
    fn a_buffer_straddling_the_end_of_a_window_is_refused() {
        let layout = Layout::from_size_align(8192, 4096).expect("layout");
        let reference = IdentityDmaPool
            .allocate_zeroed(layout)
            .expect("allocation succeeds");
        let translation = DmaTranslation::confined()
            .with_window(DmaWindow {
                physical_start: reference.phys_addr(),
                bytes: 4096,
                iova_start: IoVirtAddr::new(0x10_0000_0000),
            })
            .expect("the window fits");
        let pool = PlatformDmaPool::new(IdentityDmaPool, translation);

        let straddling = buffer_bytes(&reference, 4096 - 16, 32);
        assert_eq!(
            pool.dma_range(straddling),
            Err(DmaUnreachable {
                address: straddling.as_ptr() as usize,
                bytes: 32,
            })
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
