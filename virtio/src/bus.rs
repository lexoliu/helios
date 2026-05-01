use alloc::alloc::{alloc_zeroed, dealloc};
use core::alloc::Layout;
use core::ptr::NonNull;

use helios_hal::io::{IoError, IoResult};

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

pub trait DmaPool: Send + Sync + 'static {
    type Buffer: DmaBuffer;

    fn allocate_zeroed(&self, layout: Layout) -> IoResult<Self::Buffer>;
    fn dma_addr(&self, ptr: *const u8) -> IoResult<u64>;
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
