//! The register window an interface drives.

use core::ptr::NonNull;

use helios_hal::mmio;

/// How many bytes one locality's registers span, for both interfaces.
///
/// The TCG PC Client Platform TPM Profile places the localities 4 KiB
/// apart ("TPM Locality Address Mapping": locality 0 at `FED4_0xxxh`,
/// locality 1 at `FED4_1xxxh`), and QEMU maps exactly that much for its
/// CRB (`TPM_CRB_ADDR_SIZE`) and per locality of its FIFO
/// (`TPM_TIS_LOCALITY_SHIFT`).
pub const LOCALITY_BYTES: usize = 0x1000;

/// The bytes of one TPM locality's register page, addressed by offset.
///
/// Reads take `&mut self` as writes do: a FIFO read consumes the byte it
/// returns, and the handle is exclusively owned anyway. Every access is
/// naturally aligned and exactly as wide as its method says.
pub trait RegisterWindow: Send {
    /// How many bytes the window spans.
    fn span(&self) -> usize;

    fn read_u8(&mut self, offset: usize) -> u8;
    fn read_u32(&mut self, offset: usize) -> u32;
    fn write_u8(&mut self, offset: usize, value: u8);
    fn write_u32(&mut self, offset: usize, value: u32);
}

impl<W: RegisterWindow + ?Sized> RegisterWindow for &mut W {
    fn span(&self) -> usize {
        (**self).span()
    }

    fn read_u8(&mut self, offset: usize) -> u8 {
        (**self).read_u8(offset)
    }

    fn read_u32(&mut self, offset: usize) -> u32 {
        (**self).read_u32(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        (**self).write_u8(offset, value)
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        (**self).write_u32(offset, value)
    }
}

/// A [`RegisterWindow`] over a mapping of the TPM's register page, every
/// access going through [`helios_hal::mmio`].
pub struct MmioWindow {
    base: NonNull<u8>,
    len: usize,
}

// SAFETY: the window is a device mapping owned by one task; moving the
// pointer to another processor moves the only accessor with it.
unsafe impl Send for MmioWindow {}

impl MmioWindow {
    /// Wraps `len` bytes of device registers mapped at `base`.
    ///
    /// # Safety
    ///
    /// `base..base + len` must be a device-memory mapping of one TPM
    /// locality's registers, including any command and response buffer
    /// the interface places there, that stays mapped for the life of the
    /// window and that nothing else accesses.
    pub const unsafe fn new(base: NonNull<u8>, len: usize) -> Self {
        Self { base, len }
    }

    fn register(&self, offset: usize, width: usize) -> *mut u8 {
        assert!(
            offset.is_multiple_of(width) && offset + width <= self.len,
            "TPM register access of {width} bytes at {offset:#x} is outside the {:#x}-byte window",
            self.len
        );
        // SAFETY: the offset is inside the mapping `new` was given.
        unsafe { self.base.as_ptr().add(offset) }
    }
}

impl RegisterWindow for MmioWindow {
    fn span(&self) -> usize {
        self.len
    }

    fn read_u8(&mut self, offset: usize) -> u8 {
        // SAFETY: `register` bounds and aligns the access inside the
        // mapping `new` promised.
        unsafe { mmio::read_u8(self.register(offset, 1)) }
    }

    fn read_u32(&mut self, offset: usize) -> u32 {
        // SAFETY: as above.
        unsafe { mmio::read_u32(self.register(offset, 4).cast()) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        // SAFETY: as above.
        unsafe { mmio::write_u8(self.register(offset, 1), value) }
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        // SAFETY: as above.
        unsafe { mmio::write_u32(self.register(offset, 4).cast(), value) }
    }
}
