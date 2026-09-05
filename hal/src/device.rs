//! Hardware contracts for devices the kernel hands to one owner whole.
//!
//! A device the kernel does not drive itself is still described by the
//! platform: it occupies physical ranges, raises interrupts on named
//! sources, and — when it masters the bus — issues addresses of its own.
//! This module owns that vocabulary. It says what the *hardware* can do
//! and never who ends up owning it: a region is a physical range with
//! the access rules the silicon imposes, not a mapping; a DMA capability
//! is the device's addressing reach, not a buffer.
//!
//! # Concurrency contract
//!
//! Every type here is a plain value, `Copy` where its size allows, and
//! carries no interior state; the traits report facts a backend
//! established during bring-up and are safe to call from any processor.

use crate::interrupt::InterruptSource;
use crate::iommu::{DmaTranslation, DomainId, EndpointId, PhysicalRange};
use crate::pmm::PhysFrame;

pub trait MemoryMappedDevice: Send + Sync + 'static {
    fn base_address(&self) -> usize;

    fn span_bytes(&self) -> usize;
}

pub trait InterruptRoutedDevice: MemoryMappedDevice {
    type Interrupt: InterruptSource;

    fn interrupt_source(&self) -> Self::Interrupt;
}

/// How the hardware requires a physical range to be accessed.
///
/// The distinction is not a preference: a register file read through a
/// cacheable mapping returns whatever the line last held, and a write
/// that the store buffer merges with its neighbour reaches the device as
/// one access instead of two. The kernel therefore has to be told which
/// of the two a range is before it can map it anywhere.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// A register file. Accesses reach the device in program order,
    /// exactly as wide as the instruction issued them, and are neither
    /// cached, merged, speculated, nor replayed.
    Device,
    /// Memory the device exposes and the processor may treat as RAM: a
    /// framebuffer, an option ROM, a prefetchable aperture. Cacheable
    /// and reorderable.
    Normal,
}

/// The access rules one physical range carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRegionAttributes {
    /// Whether the range is a register file or ordinary memory.
    pub kind: MemoryKind,
    /// Whether the owner may write it. A read-only range is an option
    /// ROM or a capability window the device latches itself.
    pub writable: bool,
    /// Whether reading past what was asked for is harmless. A
    /// prefetchable aperture has no read side effects; a register file
    /// does, which is why speculation has to stay off it.
    pub prefetchable: bool,
}

impl DeviceRegionAttributes {
    /// A writable register file: the common case for a device's control
    /// registers.
    pub const REGISTERS: Self = Self {
        kind: MemoryKind::Device,
        writable: true,
        prefetchable: false,
    };

    /// A register file the owner may only read.
    pub const READ_ONLY_REGISTERS: Self = Self {
        writable: false,
        ..Self::REGISTERS
    };

    /// A prefetchable memory aperture: a framebuffer or a windowed RAM
    /// the device publishes.
    pub const PREFETCHABLE_MEMORY: Self = Self {
        kind: MemoryKind::Normal,
        writable: true,
        prefetchable: true,
    };
}

/// One physical range of a device, with the rules for reaching it.
///
/// A region names silicon, so its bounds come from the platform's own
/// description of the machine — a PCI base address register, a device
/// tree `reg` property, an ACPI resource — never from whoever wants to
/// drive it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceRegion {
    /// The physical bytes the region covers.
    pub physical: PhysicalRange,
    /// How the hardware requires them to be accessed.
    pub attributes: DeviceRegionAttributes,
}

impl DeviceRegion {
    pub const fn new(physical: PhysicalRange, attributes: DeviceRegionAttributes) -> Self {
        Self {
            physical,
            attributes,
        }
    }

    /// Whether the region starts and ends on a frame boundary.
    ///
    /// A region that does not cannot be mapped on its own: the page it
    /// shares with its neighbour would carry the neighbour's bytes into
    /// the same mapping, and the neighbour may belong to another device.
    pub const fn is_frame_aligned(&self) -> bool {
        let size = PhysFrame::SIZE as u64;
        self.physical.start.is_multiple_of(size) && self.physical.bytes.is_multiple_of(size)
    }

    /// How many frames the region spans.
    pub const fn frame_count(&self) -> usize {
        let size = PhysFrame::SIZE as u64;
        self.physical.bytes.div_ceil(size) as usize
    }

    /// The first frame of the region.
    ///
    /// # Panics
    ///
    /// Panics when the region does not start on a frame boundary; a
    /// caller that has not checked [`DeviceRegion::is_frame_aligned`]
    /// would otherwise silently map from the frame below.
    pub const fn first_frame(&self) -> PhysFrame {
        PhysFrame::from_phys_addr(self.physical.start as usize)
    }
}

/// What one device can do when it masters the bus itself.
///
/// These are hardware facts a backend reads out of the platform's
/// description: how many address bits the device drives, whether its
/// traffic snoops the processor caches, and how the addresses it issues
/// are translated on the way to memory. Nothing here says how much
/// memory anyone is willing to give it — that is a policy the kernel
/// owns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaCapability {
    /// How many low bits of an address the device can drive. A 32-bit
    /// engine on a machine with memory above 4 GiB can only reach
    /// buffers below its limit, so the kernel has to allocate under it
    /// rather than discover the truncation as corruption.
    pub address_bits: u32,
    /// Whether the device's traffic is coherent with the processor
    /// caches. When it is not, the owner has to clean and invalidate
    /// around every descriptor it hands over.
    pub coherent: bool,
    /// How the addresses the device issues become physical addresses.
    pub translation: DmaTranslation,
}

impl DmaCapability {
    /// The highest address the device can drive, inclusive.
    pub const fn address_limit(&self) -> u64 {
        assert!(
            self.address_bits != 0 && self.address_bits <= u64::BITS,
            "a bus-mastering device drives between 1 and 64 address bits"
        );
        if self.address_bits == u64::BITS {
            u64::MAX
        } else {
            (1u64 << self.address_bits) - 1
        }
    }

    /// Whether a `bytes`-long buffer at `physical` is entirely within
    /// the device's addressing reach.
    pub const fn can_reach(&self, physical: u64, bytes: u64) -> bool {
        assert!(bytes != 0, "a DMA buffer covers at least one byte");
        match physical.checked_add(bytes - 1) {
            Some(last) => last <= self.address_limit(),
            None => false,
        }
    }
}

/// A device that masters the bus, and the reach it does it with.
///
/// Implemented by a backend's handle on a discovered device, so the
/// kernel can ask a device what it is capable of without knowing which
/// bus it was found on.
pub trait DmaCapable {
    fn dma_capability(&self) -> DmaCapability;
}

/// The confinement one device sits in, when the platform has a
/// translation unit to confine it with.
///
/// A device with a domain reaches only what its domain maps; a device
/// without one reaches all of memory, and the kernel's isolation of
/// whoever drives it stops at that device's own faults. Which of the
/// two a machine offers is a property of the machine, so it is named
/// here rather than assumed anywhere above.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IommuDomain {
    /// The domain the device's translations are looked up in.
    pub domain: DomainId,
    /// The device's own identity in the unit's topology.
    pub endpoint: EndpointId,
}

impl IommuDomain {
    pub const fn new(domain: DomainId, endpoint: EndpointId) -> Self {
        Self { domain, endpoint }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceRegion, DeviceRegionAttributes, DmaCapability, MemoryKind};
    use crate::iommu::{DmaTranslation, PhysicalRange};

    fn capability(address_bits: u32) -> DmaCapability {
        DmaCapability {
            address_bits,
            coherent: true,
            translation: DmaTranslation::direct(),
        }
    }

    #[test]
    fn a_read_only_register_file_keeps_every_other_register_rule() {
        let attributes = DeviceRegionAttributes::READ_ONLY_REGISTERS;

        assert_eq!(attributes.kind, MemoryKind::Device);
        assert!(!attributes.writable);
        assert!(!attributes.prefetchable);
    }

    #[test]
    fn a_region_that_shares_a_frame_with_its_neighbour_is_not_frame_aligned() {
        let aligned = DeviceRegion::new(
            PhysicalRange::new(0x4000_0000, 0x2000),
            DeviceRegionAttributes::REGISTERS,
        );
        let straddling = DeviceRegion::new(
            PhysicalRange::new(0x4000_0800, 0x800),
            DeviceRegionAttributes::REGISTERS,
        );

        assert!(aligned.is_frame_aligned());
        assert_eq!(aligned.frame_count(), 2);
        assert_eq!(aligned.first_frame().phys_addr(), 0x4000_0000);
        assert!(!straddling.is_frame_aligned());
    }

    #[test]
    fn a_thirty_two_bit_engine_cannot_reach_above_its_limit() {
        let engine = capability(32);

        assert_eq!(engine.address_limit(), 0xffff_ffff);
        assert!(engine.can_reach(0xffff_f000, 0x1000));
        assert!(!engine.can_reach(0xffff_f001, 0x1000));
        assert!(!engine.can_reach(0x1_0000_0000, 0x1000));
    }

    #[test]
    fn a_sixty_four_bit_engine_reaches_the_top_of_memory_without_overflowing() {
        let engine = capability(64);

        assert_eq!(engine.address_limit(), u64::MAX);
        assert!(engine.can_reach(u64::MAX - 0xfff, 0x1000));
        assert!(!engine.can_reach(u64::MAX, 2));
    }
}
