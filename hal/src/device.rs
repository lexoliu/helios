//! What a device is, as hardware.
//!
//! These are capability types: they say what a piece of hardware
//! presents and what it may do to memory, and nothing about who drives
//! it. A driver that lives in the kernel and a driver that lives in an
//! isolated user-mode instance are described by the same values here;
//! the difference is entirely in what the layers above do with them.

use crate::interrupt::InterruptSource;
use crate::iommu::DomainId;
use crate::pmm::{PhysFrame, PhysFrameRange};

pub trait MemoryMappedDevice: Send + Sync + 'static {
    fn base_address(&self) -> usize;

    fn span_bytes(&self) -> usize;
}

pub trait InterruptRoutedDevice: MemoryMappedDevice {
    type Interrupt: InterruptSource;

    fn interrupt_source(&self) -> Self::Interrupt;
}

/// How a physical window must be accessed.
///
/// Register windows are not memory: the processor may not merge, split,
/// reorder or speculatively repeat accesses to them, and a cache line
/// holding a register is a bug rather than an optimisation. The
/// distinction reaches the page tables, which is why it is part of the
/// region rather than a convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RegionAttribute {
    /// Device memory: non-gathering, non-reordering, non-early-write
    /// acknowledgement. What a register window needs.
    Registers,
    /// Ordinary cacheable memory. What a device's on-board RAM or a
    /// shared descriptor area needs.
    Memory,
}

/// One physical window a device presents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRegion {
    pub physical_base: usize,
    pub byte_len: usize,
    pub attribute: RegionAttribute,
}

impl DeviceRegion {
    /// A register window, which is what a BAR or a device-tree `reg`
    /// entry describes.
    pub const fn registers(physical_base: usize, byte_len: usize) -> Self {
        Self {
            physical_base,
            byte_len,
            attribute: RegionAttribute::Registers,
        }
    }

    /// The frames this window covers, rounded out to page boundaries.
    ///
    /// A window is mapped a page at a time whatever its declared
    /// length, so a register block that ends mid-page still needs the
    /// whole page — and the caller has to know that the extra bytes are
    /// reachable rather than discover it later.
    pub const fn frames(self) -> PhysFrameRange {
        let start = self.physical_base & !(PhysFrame::SIZE - 1);
        let end = self
            .physical_base
            .saturating_add(self.byte_len)
            .next_multiple_of(PhysFrame::SIZE);
        PhysFrameRange::from_phys_addr(start, end - start)
    }

    /// Where the window's first declared byte sits inside the first
    /// mapped page.
    pub const fn page_offset(self) -> usize {
        self.physical_base & (PhysFrame::SIZE - 1)
    }
}

/// What a device may do to memory on its own behalf.
///
/// A device that reads and writes memory without the processor's help
/// is trusted with whatever its DMA can reach. That is a property of
/// the platform, not of the driver: an endpoint the firmware placed in
/// an IOMMU domain reaches only what the domain maps, and one the
/// platform left unconfined reaches all of physical memory whoever
/// programs it. Both are described here so the layer that hands the
/// device to a driver can say which it is handing over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaCapable {
    /// Physical address bits the device's DMA engine can drive. A
    /// buffer above `1 << bits` is unreachable for it whatever the
    /// page tables say.
    pub address_width_bits: u8,
    /// The translation domain the platform put this endpoint in, where
    /// the platform has an IOMMU. `None` is not "no DMA": it is DMA
    /// that reaches all of physical memory.
    pub domain: Option<DomainId>,
}

impl DmaCapable {
    /// The common case for a 64-bit-capable endpoint on a platform with
    /// no translation unit.
    pub const UNCONFINED_64: Self = Self {
        address_width_bits: 64,
        domain: None,
    };

    /// Whether the platform confines this device's DMA.
    pub const fn is_confined(self) -> bool {
        self.domain.is_some()
    }

    /// Whether `physical` is an address this device can actually drive.
    pub const fn can_address(self, physical: usize) -> bool {
        if self.address_width_bits >= usize::BITS as u8 {
            return true;
        }
        (physical as u64) < (1_u64 << self.address_width_bits)
    }
}
