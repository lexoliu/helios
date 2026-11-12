use alloc::alloc::{alloc, alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::future::Future;
use core::ptr::NonNull;

use futures_lite::future::yield_now;
use helios_hal::dma::{DeviceAddress, DmaAddressing, DmaBuffer, DmaError, DmaMemory, DmaSpaceMark};
use helios_hal::io::{IoError, IoResult};
use helios_hal::iommu::{DmaAccess, DmaTranslation};
use helios_hal::mmio;
use helios_hal::pmm::PhysFrame;

pub trait DeviceBus: Send + Sync + 'static {
    type DmaPool: DmaMemory;

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

/// The largest run the kernel heap can be asked for: `isize::MAX` rounded
/// down to a page, the most [`Layout`] accepts at page alignment.
const HEAP_RUN_BYTES: usize = isize::MAX as usize & !(PhysFrame::SIZE - 1);

impl IdentityDmaPool {
    fn carve(&self, layout: Layout, zeroed: bool) -> Result<IdentityDmaBuffer, DmaError> {
        // SAFETY: the callers pass non-zero-sized layouts, as the
        // allocator requires.
        let ptr = unsafe {
            if zeroed {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };
        let ptr = NonNull::new(ptr).ok_or(DmaError::Exhausted {
            bytes: layout.size(),
        })?;
        Ok(IdentityDmaBuffer { ptr, layout })
    }
}

impl DmaMemory for IdentityDmaPool {
    type Buffer = IdentityDmaBuffer;

    fn allocate_resident(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.allocate(layout, access)
    }

    fn allocate(&self, layout: Layout, _access: DmaAccess) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, true)
    }

    /// The heap's allocation, without the pass that zeroes it.
    fn allocate_overwritten(
        &self,
        layout: Layout,
        _access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, false)
    }

    /// One heap allocation: as long as a layout can describe.
    fn max_run_bytes(&self) -> usize {
        HEAP_RUN_BYTES
    }

    /// Heap memory has no signal for space coming back: a refusal is not
    /// for good, because the allocator can satisfy the same request once
    /// anything frees memory, but nothing announces it. There is no
    /// generation to sample.
    fn want_space(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    /// A wait is one yield to the executor: a caller that asks again
    /// polls the heap at the executor's pace, never in a loop that
    /// cannot return `Pending`.
    async fn space_changed(&self, _mark: DmaSpaceMark) {
        yield_now().await;
    }

    fn short_of_space(&self) -> bool {
        false
    }

    fn space_mark(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError> {
        Ok(DeviceAddress::new(bytes.as_ptr() as usize as u64))
    }

    /// Physical addresses and host addresses are the same thing in this
    /// pool, so a run of somebody else's memory is reached where it is.
    fn foreign_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    fn addressing(&self) -> DmaAddressing {
        DmaAddressing::Physical
    }
}

impl OffsetDmaPool {
    fn carve(&self, layout: Layout, zeroed: bool) -> Result<OffsetDmaBuffer, DmaError> {
        // SAFETY: the callers pass non-zero-sized layouts, as the
        // allocator requires.
        let ptr = unsafe {
            if zeroed {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        };
        let ptr = NonNull::new(ptr).ok_or(DmaError::Exhausted {
            bytes: layout.size(),
        })?;
        let physical_address = self.translate(ptr.as_ptr()).map_err(|_| {
            // SAFETY: `ptr` was allocated above with `layout` and is not
            // used again.
            unsafe { dealloc(ptr.as_ptr(), layout) };
            DmaError::Unreachable {
                address: ptr.as_ptr() as usize,
                bytes: layout.size(),
            }
        })?;
        Ok(OffsetDmaBuffer {
            ptr,
            layout,
            physical_address,
        })
    }
}

impl DmaMemory for OffsetDmaPool {
    type Buffer = OffsetDmaBuffer;

    fn allocate_resident(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.allocate(layout, access)
    }

    fn allocate(&self, layout: Layout, _access: DmaAccess) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, true)
    }

    /// The heap's allocation, without the pass that zeroes it.
    fn allocate_overwritten(
        &self,
        layout: Layout,
        _access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.carve(layout, false)
    }

    /// One heap allocation, contiguous in physical memory through the
    /// linear offset: as long as a layout can describe.
    fn max_run_bytes(&self) -> usize {
        HEAP_RUN_BYTES
    }

    /// Heap memory has no signal for space coming back: a refusal is not
    /// for good, because the allocator can satisfy the same request once
    /// anything frees memory, but nothing announces it. There is no
    /// generation to sample.
    fn want_space(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    /// A wait is one yield to the executor: a caller that asks again
    /// polls the heap at the executor's pace, never in a loop that
    /// cannot return `Pending`.
    async fn space_changed(&self, _mark: DmaSpaceMark) {
        yield_now().await;
    }

    fn short_of_space(&self) -> bool {
        false
    }

    fn space_mark(&self) -> DmaSpaceMark {
        DmaSpaceMark::new(0)
    }

    /// The device sees physical addresses, so a run named by one is
    /// already the address it has to issue. The offset this pool holds
    /// turns a kernel pointer into a physical address and has nothing
    /// to say about memory that arrived as one.
    fn foreign_range(&self, physical: u64, _bytes: u64) -> IoResult<u64> {
        Ok(physical)
    }

    /// The offset is linear, so a buffer is contiguous in physical memory
    /// exactly when it is in the kernel's address space.
    fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError> {
        self.translate(bytes.as_ptr())
            .map(DeviceAddress::new)
            .map_err(|_| DmaError::Unreachable {
                address: bytes.as_ptr() as usize,
                bytes: bytes.len(),
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
    device_address: DeviceAddress,
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

impl<P: DmaMemory> PlatformDmaPool<P> {
    /// `inner` with the address the device issues for it.
    fn translated(&self, inner: P::Buffer) -> Result<PlatformDmaBuffer<P::Buffer>, DmaError> {
        let unreachable = DmaError::Unreachable {
            address: inner.as_ptr() as usize,
            bytes: inner.len(),
        };
        let bytes = u64::try_from(inner.len()).map_err(|_| unreachable)?;
        let device_address = self
            .translation
            .device_range(inner.device_address().get(), bytes)
            .map_err(|_| unreachable)?;
        Ok(PlatformDmaBuffer {
            inner,
            device_address: DeviceAddress::new(device_address),
        })
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

impl<P: DmaMemory> DmaMemory for PlatformDmaPool<P> {
    type Buffer = PlatformDmaBuffer<P::Buffer>;

    fn allocate_resident(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.translated(self.inner.allocate_resident(layout, access)?)
    }

    fn allocate(&self, layout: Layout, access: DmaAccess) -> Result<Self::Buffer, DmaError> {
        self.translated(self.inner.allocate(layout, access)?)
    }

    fn allocate_overwritten(
        &self,
        layout: Layout,
        access: DmaAccess,
    ) -> Result<Self::Buffer, DmaError> {
        self.translated(self.inner.allocate_overwritten(layout, access)?)
    }

    /// What the wrapped memory serves: the translation maps what the
    /// inner pool hands out, and refuses a run it cannot map contiguously
    /// when the run is translated.
    fn max_run_bytes(&self) -> usize {
        self.inner.max_run_bytes()
    }

    fn want_space(&self) -> DmaSpaceMark {
        self.inner.want_space()
    }

    fn space_changed(&self, mark: DmaSpaceMark) -> impl Future<Output = ()> + Send + '_ {
        self.inner.space_changed(mark)
    }

    fn short_of_space(&self) -> bool {
        self.inner.short_of_space()
    }

    fn space_mark(&self) -> DmaSpaceMark {
        self.inner.space_mark()
    }

    /// The whole slice has to sit inside one window of the translation.
    fn device_range(&self, bytes: &[u8]) -> Result<DeviceAddress, DmaError> {
        let unreachable = DmaError::Unreachable {
            address: bytes.as_ptr() as usize,
            bytes: bytes.len(),
        };
        let physical = self.inner.device_range(bytes)?.get();
        let length = u64::try_from(bytes.len()).map_err(|_| unreachable)?;
        self.translation
            .device_range(physical, length)
            .map(DeviceAddress::new)
            .map_err(|_| unreachable)
    }

    fn foreign_range(&self, physical: u64, bytes: u64) -> IoResult<u64> {
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
    fn device_address(&self) -> DeviceAddress {
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
    fn device_address(&self) -> DeviceAddress {
        DeviceAddress::new(self.ptr.as_ptr() as usize as u64)
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.layout.size()
    }
}

impl DmaBuffer for OffsetDmaBuffer {
    fn device_address(&self) -> DeviceAddress {
        DeviceAddress::new(self.physical_address)
    }

    fn as_ptr(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    fn len(&self) -> usize {
        self.layout.size()
    }
}

impl AsRef<[u8]> for IdentityDmaBuffer {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the buffer owns `layout.size()` zero-initialised bytes
        // for as long as it lives.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl AsMut<[u8]> for IdentityDmaBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` is the only way to this memory
        // from the CPU.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl AsRef<[u8]> for OffsetDmaBuffer {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: the buffer owns `layout.size()` zero-initialised bytes
        // for as long as it lives.
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl AsMut<[u8]> for OffsetDmaBuffer {
    fn as_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` is the only way to this memory
        // from the CPU.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for PlatformDmaBuffer<B> {
    fn as_ref(&self) -> &[u8] {
        self.inner.as_ref()
    }
}

impl<B: AsMut<[u8]>> AsMut<[u8]> for PlatformDmaBuffer<B> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.inner.as_mut()
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

impl<P: DmaMemory> DeviceBus for MmioBus<P> {
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
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::alloc::Layout;
    use core::future::Future;
    use core::pin::pin;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use core::task::{Context, Poll, Waker};

    use helios_hal::dma::{DeviceAddress, DmaAddressing, DmaBuffer, DmaError, DmaMemory};
    use helios_hal::iommu::{DmaAccess, DmaTranslation, DmaWindow, IoVirtAddr};

    use super::{IdentityDmaPool, OffsetDmaPool, PlatformDmaPool};

    /// A platform with no translation unit hands the device physical
    /// addresses and must not ask it to negotiate anything.
    #[test]
    fn a_direct_platform_pool_hands_out_physical_addresses() {
        let pool = PlatformDmaPool::new(IdentityDmaPool, DmaTranslation::direct());
        let layout = Layout::from_size_align(256, 16).expect("layout");
        let buffer = pool
            .allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
            .expect("allocation succeeds");

        assert_eq!(pool.addressing(), DmaAddressing::Physical);
        assert_eq!(
            buffer.device_address(),
            DeviceAddress::new(buffer.as_ptr() as usize as u64)
        );
        assert_eq!(
            pool.device_range(buffer_bytes(&buffer, 0, 256))
                .expect("translation"),
            DeviceAddress::new(buffer.as_ptr() as usize as u64)
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
            .allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
            .expect("allocation succeeds");
        let physical = reference.device_address().get();
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
            pool.device_range(buffer_bytes(&reference, 0, 16))
                .expect("translation"),
            DeviceAddress::new(0x10_0000_0000)
        );
        assert_eq!(
            pool.device_range(buffer_bytes(&reference, 0x40, 16))
                .expect("translation"),
            DeviceAddress::new(0x10_0000_0040)
        );
        assert_eq!(
            pool.device_range(buffer_bytes(&reference, 0, 4096))
                .expect("a slice that fills the window translates"),
            DeviceAddress::new(0x10_0000_0000)
        );
    }

    /// A slice that starts inside a window and runs past its end used to
    /// translate by its first byte. It is refused, with the address.
    #[test]
    fn a_buffer_straddling_the_end_of_a_window_is_refused() {
        let layout = Layout::from_size_align(8192, 4096).expect("layout");
        let reference = IdentityDmaPool
            .allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
            .expect("allocation succeeds");
        let translation = DmaTranslation::confined()
            .with_window(DmaWindow {
                physical_start: reference.device_address().get(),
                bytes: 4096,
                iova_start: IoVirtAddr::new(0x10_0000_0000),
            })
            .expect("the window fits");
        let pool = PlatformDmaPool::new(IdentityDmaPool, translation);

        let straddling = buffer_bytes(&reference, 4096 - 16, 32);
        assert_eq!(
            pool.device_range(straddling),
            Err(DmaError::Unreachable {
                address: straddling.as_ptr() as usize,
                bytes: 32,
            })
        );
    }

    /// Counts its wakes, so a test can tell a yield from a park nobody
    /// will end.
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Polls `wait` twice and checks the shape of a yield: pending on the
    /// first poll with the task woken, ready on the second.
    fn assert_is_one_yield(wait: impl Future<Output = ()>) {
        let count = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(count.clone());
        let mut context = Context::from_waker(&waker);
        let mut wait = pin!(wait);

        assert_eq!(wait.as_mut().poll(&mut context), Poll::Pending);
        assert_eq!(
            count.0.load(Ordering::SeqCst),
            1,
            "a pending wait woke its task, or nothing would poll it again"
        );
        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
    }

    /// Heap memory announces nothing when it comes back, so a caller that
    /// follows `want_space` and parks on `space_changed` has to be handed
    /// back to the executor, or its retry loop never returns `Pending`.
    #[test]
    fn waiting_for_heap_memory_yields_once() {
        let identity = IdentityDmaPool;
        assert_is_one_yield(identity.space_changed(identity.want_space()));
        let offset = OffsetDmaPool::new(0);
        assert_is_one_yield(offset.space_changed(offset.want_space()));
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

        assert!(matches!(
            pool.allocate(layout, DmaAccess::READ | DmaAccess::WRITE)
                .err(),
            Some(DmaError::Unreachable { bytes: 64, .. })
        ));
    }
}
