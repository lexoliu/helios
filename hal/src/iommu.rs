//! Hardware contracts for I/O memory management units.
//!
//! An IOMMU sits between a DMA-capable device and memory: the device
//! issues addresses in its own I/O virtual address space and the unit
//! translates them, so a device can only reach the physical ranges its
//! translation domain maps. This module owns the vocabulary — endpoint
//! identities, domains, access rights, geometry — and the
//! [`Iommu`] trait a concrete unit satisfies. It names no device class,
//! bus or runtime: which endpoints exist is a topology question the
//! backends answer, and which domains are built is the kernel's.
//!
//! Concurrency contract: [`Iommu`] implementations are shared across
//! processors, so every method takes `&self` and must be internally
//! synchronised. Domain construction runs on the bootstrap processor
//! during device bring-up; a [`DmaTranslation`] is immutable once built
//! and is read from every processor that submits DMA.

use core::fmt;
use core::ops::RangeInclusive;

use bitflags::bitflags;
use thiserror::Error;

/// The identity a platform gives one DMA-capable device in its IOMMU
/// topology.
///
/// The number comes from the platform's own description of the machine —
/// the ACPI VIOT table or the device tree's `iommu-map` — never from the
/// driver that happens to claim the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EndpointId(u32);

impl EndpointId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#x}", self.0)
    }
}

/// One translation domain: a set of endpoints that share a page table.
///
/// Helios gives every protected device a domain of its own, so a domain
/// identifier is in practice the confinement boundary of one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DomainId(u32);

impl DomainId {
    pub const fn new(id: u32) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl fmt::Display for DomainId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// An address as a confined device issues it, before translation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct IoVirtAddr(u64);

impl IoVirtAddr {
    pub const fn new(address: u64) -> Self {
        Self(address)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for IoVirtAddr {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#x}", self.0)
    }
}

bitflags! {
    /// What a device may do with a mapped range.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DmaAccess: u32 {
        /// The device may read the range.
        const READ = 1 << 0;
        /// The device may write the range.
        const WRITE = 1 << 1;
        /// The range is device-side memory-mapped I/O rather than RAM;
        /// an interrupt doorbell is the case that matters here.
        const MMIO = 1 << 2;
    }
}

/// The address space and page-size constraints one unit imposes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IommuGeometry {
    /// Bit `n` is set when the unit can map a page of `1 << n` bytes.
    pub page_size_mask: u64,
    /// The I/O virtual addresses the unit accepts.
    pub input_range: RangeInclusive<u64>,
    /// The domain identifiers the unit accepts.
    pub domain_range: RangeInclusive<u32>,
}

impl IommuGeometry {
    /// The smallest page the unit can map, which is the alignment every
    /// mapping start and length has to meet.
    pub const fn granule(&self) -> u64 {
        let mask = self.page_size_mask;
        assert!(mask != 0, "an IOMMU must support at least one page size");
        1 << mask.trailing_zeros()
    }

    /// Rounds `address` down to the start of its granule.
    pub const fn align_down(&self, address: u64) -> u64 {
        address & !(self.granule() - 1)
    }

    /// Rounds `address` up to the next granule boundary.
    pub const fn align_up(&self, address: u64) -> u64 {
        let granule = self.granule();
        match address.checked_add(granule - 1) {
            Some(raised) => raised & !(granule - 1),
            None => panic!("IOMMU granule alignment overflowed the address space"),
        }
    }
}

/// Why an IOMMU operation could not be carried out.
///
/// The variants mirror the failure classes the virtio-iommu status codes
/// distinguish, so a concrete unit never has to collapse a device answer
/// into a generic I/O error.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum IommuError {
    #[error("the IOMMU does not implement this request")]
    Unsupported,
    #[error("the IOMMU rejected the request as invalid")]
    Invalid,
    #[error("the request named a domain, endpoint or mapping that does not exist")]
    NotFound,
    #[error("the requested range lies outside what the IOMMU accepts")]
    OutOfRange,
    #[error("the IOMMU ran out of resources for the request")]
    OutOfResources,
    #[error("the IOMMU reported a translation fault")]
    Fault,
    #[error("the IOMMU device failed the request")]
    DeviceFault,
    #[error("physical address {physical:#x} is not mapped into this device's address space")]
    Unmapped { physical: u64 },
    #[error("the address space cannot hold another translation window")]
    TooManyWindows,
}

/// A shared translation unit is still one unit: delegating lets a
/// backend keep the driver alive on its interrupt path while the kernel
/// builds domains through the same contract.
impl<I: Iommu> Iommu for alloc::sync::Arc<I> {
    fn geometry(&self) -> IommuGeometry {
        I::geometry(self)
    }

    fn attach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError> {
        I::attach(self, domain, endpoint)
    }

    fn detach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError> {
        I::detach(self, domain, endpoint)
    }

    fn map(
        &self,
        domain: DomainId,
        iova: IoVirtAddr,
        physical: u64,
        bytes: u64,
        access: DmaAccess,
    ) -> Result<(), IommuError> {
        I::map(self, domain, iova, physical, bytes, access)
    }

    fn unmap(&self, domain: DomainId, iova: IoVirtAddr, bytes: u64) -> Result<(), IommuError> {
        I::unmap(self, domain, iova, bytes)
    }
}

/// The hardware contract a translation unit satisfies.
///
/// Requests are issued during device bring-up and teardown, never on a
/// data path: helios maps a domain's memory once and then translates in
/// software through [`DmaTranslation`], so a submission never has to
/// wait on the unit.
pub trait Iommu: Send + Sync + 'static {
    /// The address space and page-size constraints of this unit.
    fn geometry(&self) -> IommuGeometry;

    /// Puts `endpoint` under `domain`'s translation. Every DMA the
    /// endpoint issues afterwards is translated through that domain.
    fn attach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError>;

    /// Removes `endpoint` from `domain`.
    fn detach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError>;

    /// Maps `bytes` bytes of physical memory starting at `physical` at
    /// `iova` in `domain`, with `access` rights.
    fn map(
        &self,
        domain: DomainId,
        iova: IoVirtAddr,
        physical: u64,
        bytes: u64,
        access: DmaAccess,
    ) -> Result<(), IommuError>;

    /// Removes the mapping `map` installed.
    fn unmap(&self, domain: DomainId, iova: IoVirtAddr, bytes: u64) -> Result<(), IommuError>;
}

impl From<IommuError> for crate::io::IoError {
    fn from(error: IommuError) -> Self {
        match error {
            IommuError::Unsupported => Self::Unsupported,
            IommuError::Invalid | IommuError::TooManyWindows => {
                Self::InvalidDeviceConfig("the IOMMU refused the request as invalid")
            }
            IommuError::NotFound => Self::NotFound,
            IommuError::OutOfRange | IommuError::Unmapped { .. } => Self::OutOfBounds,
            IommuError::OutOfResources | IommuError::Fault | IommuError::DeviceFault => {
                Self::DeviceFault
            }
        }
    }
}

/// A contiguous run of physical memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalRange {
    /// First physical byte of the range.
    pub start: u64,
    /// Length of the range in bytes.
    pub bytes: u64,
}

impl PhysicalRange {
    pub const fn new(start: u64, bytes: u64) -> Self {
        Self { start, bytes }
    }

    /// The inclusive last byte of the range.
    pub const fn last(&self) -> u64 {
        assert!(self.bytes != 0, "a physical range covers at least one byte");
        self.start + (self.bytes - 1)
    }

    /// The smallest range covering this one whose bounds sit on
    /// `geometry`'s granule.
    pub fn aligned_to(&self, geometry: &IommuGeometry) -> Self {
        let start = geometry.align_down(self.start);
        let end = self
            .last()
            .checked_add(1)
            .map(|end| geometry.align_up(end))
            .unwrap_or_else(|| panic!("physical range {self:?} ends at the top of memory"));
        Self {
            start,
            bytes: end - start,
        }
    }
}

/// One contiguous physical range as a device sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaWindow {
    /// First physical byte the window covers.
    pub physical_start: u64,
    /// Length of the window in bytes.
    pub bytes: u64,
    /// The I/O virtual address the window starts at.
    pub iova_start: IoVirtAddr,
}

impl DmaWindow {
    /// The I/O virtual address `physical` appears at, when this window
    /// covers it.
    const fn translate(&self, physical: u64) -> Option<u64> {
        let offset = physical.wrapping_sub(self.physical_start);
        if physical < self.physical_start || offset >= self.bytes {
            return None;
        }
        Some(self.iova_start.get() + offset)
    }
}

/// How many windows one device address space can describe.
///
/// A window is one contiguous run of DMA-capable physical memory plus
/// the platform's interrupt doorbells; no machine helios boots on splits
/// its usable memory into more runs than this.
pub const MAX_DMA_WINDOWS: usize = 8;

/// The translation a device's DMA addresses go through, as the platform
/// established it.
///
/// This is the value form of [`crate::DmaModel`]: on a machine with no
/// translation unit a device address *is* a physical address, and on one
/// where helios confined the device it is an I/O virtual address that
/// only resolves inside the device's own domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaTranslation {
    windows: [Option<DmaWindow>; MAX_DMA_WINDOWS],
    /// Set when the platform has no translation unit and a device
    /// address is a physical address.
    direct: bool,
}

impl DmaTranslation {
    /// The device issues physical addresses: the platform has no
    /// translation unit in the path.
    pub const fn direct() -> Self {
        Self {
            windows: [const { None }; MAX_DMA_WINDOWS],
            direct: true,
        }
    }

    /// An empty translated address space; every reachable range has to
    /// be added with [`DmaTranslation::with_window`].
    pub const fn confined() -> Self {
        Self {
            windows: [const { None }; MAX_DMA_WINDOWS],
            direct: false,
        }
    }

    /// Adds one reachable window.
    pub fn with_window(mut self, window: DmaWindow) -> Result<Self, IommuError> {
        assert!(
            !self.direct,
            "a direct DMA translation has no windows to add"
        );
        let slot = self
            .windows
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(IommuError::TooManyWindows)?;
        *slot = Some(window);
        Ok(self)
    }

    /// Whether device addresses are physical addresses.
    pub const fn is_direct(&self) -> bool {
        self.direct
    }

    /// Every window this address space reaches.
    pub fn windows(&self) -> impl Iterator<Item = &DmaWindow> {
        self.windows.iter().flatten()
    }

    /// Total number of physical bytes the device can reach.
    pub fn mapped_bytes(&self) -> u64 {
        self.windows().map(|window| window.bytes).sum()
    }

    /// The address the device has to issue to reach the `bytes`-long
    /// range at `physical`.
    ///
    /// The whole range has to sit inside one window: a buffer that
    /// straddled two of them would be contiguous in physical memory but
    /// not in the device's address space.
    pub fn device_range(&self, physical: u64, bytes: u64) -> Result<u64, IommuError> {
        assert!(bytes != 0, "a DMA range covers at least one byte");
        if self.direct {
            return Ok(physical);
        }
        let last = physical
            .checked_add(bytes - 1)
            .ok_or(IommuError::OutOfRange)?;
        let start = self.device_address(physical)?;
        let end = self.device_address(last)?;
        if end - start != bytes - 1 {
            return Err(IommuError::Unmapped { physical });
        }
        Ok(start)
    }

    /// The address the device has to issue to reach `physical`.
    ///
    /// A physical address outside every window is an error rather than a
    /// pass-through: the device would fault on it, and failing here
    /// names the buffer instead of the fault.
    pub fn device_address(&self, physical: u64) -> Result<u64, IommuError> {
        if self.direct {
            return Ok(physical);
        }
        self.windows()
            .find_map(|window| window.translate(physical))
            .ok_or(IommuError::Unmapped { physical })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DmaTranslation, DmaWindow, EndpointId, IoVirtAddr, IommuError, IommuGeometry,
        MAX_DMA_WINDOWS,
    };

    fn geometry(page_size_mask: u64) -> IommuGeometry {
        IommuGeometry {
            page_size_mask,
            input_range: 0..=u64::MAX,
            domain_range: 0..=u32::MAX,
        }
    }

    #[test]
    fn the_granule_is_the_smallest_page_the_unit_offers() {
        // `-(4 * KiB)`: every power of two from 4 KiB upwards.
        assert_eq!(geometry(0xffff_ffff_ffff_f000).granule(), 4096);
        // A host-page granule on a 16 KiB machine.
        assert_eq!(geometry(0xffff_ffff_ffff_c000).granule(), 16384);
    }

    #[test]
    fn alignment_follows_the_granule() {
        let geometry = geometry(0xffff_ffff_ffff_c000);

        assert_eq!(geometry.align_down(0x4001), 0x4000);
        assert_eq!(geometry.align_up(0x4001), 0x8000);
        assert_eq!(geometry.align_up(0x4000), 0x4000);
    }

    #[test]
    fn a_direct_translation_hands_back_the_physical_address() {
        let translation = DmaTranslation::direct();

        assert!(translation.is_direct());
        assert_eq!(translation.device_address(0x1234).expect("direct"), 0x1234);
        assert_eq!(translation.mapped_bytes(), 0);
    }

    #[test]
    fn a_confined_translation_maps_only_its_windows() {
        let translation = DmaTranslation::confined()
            .with_window(DmaWindow {
                physical_start: 0x4000_0000,
                bytes: 0x1000_0000,
                iova_start: IoVirtAddr::new(0x1_0000_0000),
            })
            .expect("the first window fits")
            .with_window(DmaWindow {
                physical_start: 0xfee0_0000,
                bytes: 0x10_0000,
                iova_start: IoVirtAddr::new(0xfee0_0000),
            })
            .expect("the doorbell window fits");

        assert!(!translation.is_direct());
        assert_eq!(
            translation.device_address(0x4000_0000).expect("mapped"),
            0x1_0000_0000
        );
        assert_eq!(
            translation.device_address(0x4000_1234).expect("mapped"),
            0x1_0000_1234
        );
        // An identity-mapped doorbell keeps its physical address.
        assert_eq!(
            translation.device_address(0xfee0_0000).expect("mapped"),
            0xfee0_0000
        );
        assert_eq!(translation.mapped_bytes(), 0x1000_0000 + 0x10_0000);
    }

    #[test]
    fn an_unmapped_physical_address_is_refused_instead_of_passed_through() {
        let translation = DmaTranslation::confined()
            .with_window(DmaWindow {
                physical_start: 0x4000_0000,
                bytes: 0x1000,
                iova_start: IoVirtAddr::new(0x1_0000_0000),
            })
            .expect("the window fits");

        assert_eq!(
            translation.device_address(0x4000_1000),
            Err(IommuError::Unmapped {
                physical: 0x4000_1000
            })
        );
        assert_eq!(
            translation.device_address(0x3fff_ffff),
            Err(IommuError::Unmapped {
                physical: 0x3fff_ffff
            })
        );
    }

    #[test]
    fn an_address_space_refuses_more_windows_than_it_can_hold() {
        let mut translation = DmaTranslation::confined();
        for index in 0..MAX_DMA_WINDOWS {
            translation = translation
                .with_window(DmaWindow {
                    physical_start: index as u64 * 0x1000,
                    bytes: 0x1000,
                    iova_start: IoVirtAddr::new(index as u64 * 0x1000),
                })
                .expect("windows up to the capacity fit");
        }

        assert_eq!(
            translation.with_window(DmaWindow {
                physical_start: 0xdead_0000,
                bytes: 0x1000,
                iova_start: IoVirtAddr::new(0xdead_0000),
            }),
            Err(IommuError::TooManyWindows)
        );
    }

    #[test]
    fn endpoint_identifiers_render_as_the_topology_writes_them() {
        assert_eq!(alloc::format!("{}", EndpointId::new(0x0018)), "0x18");
    }
}
