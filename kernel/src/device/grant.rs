//! What one device instance is, as the kernel hands it away.

use arrayvec::{ArrayString, ArrayVec};
use helios_hal::device::{DeviceRegion, DmaCapability, IommuDomain};
use helios_hal::interrupt::InterruptSource;
use thiserror::Error;

/// Regions one grant carries.
///
/// Six is the number of base address registers a PCI function has, and
/// no device tree node in the tree describes more ranges than that for
/// one device either.
pub const MAX_GRANT_REGIONS: usize = 6;

/// Interrupts one grant carries.
///
/// A function with per-queue messages takes one per queue plus one for
/// configuration changes; eight covers the non-virtio hardware a grant
/// is for without letting one device's routes crowd out another's.
pub const MAX_GRANT_INTERRUPTS: usize = 8;

/// Longest device name the registry stores.
///
/// A name is the platform's own path to the device — `pci:0000:00:04.0`,
/// or a device tree node path — so it is bounded by what firmware
/// writes, not by anything the kernel chooses.
pub const MAX_DEVICE_NAME: usize = 64;

/// The platform's own path to one device.
///
/// Names are compared, never parsed: the kernel matches the name a
/// driver asks for against the name discovery published and does not
/// interpret either.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceName(ArrayString<MAX_DEVICE_NAME>);

impl DeviceName {
    /// The name `text` spells, or [`GrantError::NameTooLong`] when
    /// firmware named a device more verbosely than the registry stores.
    pub fn new(text: &str) -> Result<Self, GrantError> {
        ArrayString::from(text)
            .map(Self)
            .map_err(|_| GrantError::NameTooLong)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for DeviceName {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The platform's own number for one interrupt a granted device raises.
///
/// The kernel keeps the raw number rather than the backend's
/// [`InterruptSource`] type so a grant does not drag the controller's
/// vocabulary through every layer that carries one; the number goes back
/// to the backend unchanged when the source has to be masked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GrantInterrupt(u32);

impl GrantInterrupt {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The number a backend's own [`InterruptSource`] carries.
    pub fn from_source<Source: InterruptSource>(source: Source) -> Self {
        Self(source.raw())
    }

    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl core::fmt::Display for GrantInterrupt {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// How much memory the kernel is willing to pin for one device, and
/// what that device can reach with it.
///
/// The capability is a hardware fact the backend read out of the
/// platform; the budget is a policy the kernel sets, so a driver that
/// asks for rings the machine cannot afford is refused rather than
/// allowed to squeeze every other instance out of the user pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DmaBudget {
    /// What the device can reach when it masters the bus.
    pub capability: DmaCapability,
    /// Most bytes the kernel will pin for it at once.
    pub byte_budget: u64,
}

/// Why a grant could not be built, published, claimed or used.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum GrantError {
    #[error("the device name is longer than the registry stores")]
    NameTooLong,
    #[error("a grant carries at most {MAX_GRANT_REGIONS} regions")]
    TooManyRegions,
    #[error("a grant carries at most {MAX_GRANT_INTERRUPTS} interrupts")]
    TooManyInterrupts,
    #[error("the region does not start and end on a frame boundary")]
    RegionNotFrameAligned,
    #[error("the machine can publish no more device grants")]
    RegistryFull,
    #[error("a device with this name is already published")]
    DuplicateName,
    #[error("no device with this name was discovered")]
    NotFound,
    #[error("the device is already granted to another owner")]
    AlreadyClaimed,
    #[error("the platform surface a granted device needs is not installed")]
    PlatformUnavailable,
    #[error("the grant has no region with this index")]
    NoSuchRegion,
    #[error("the grant has no interrupt with this index")]
    NoSuchInterrupt,
    #[error("the region is already mapped into the owner's memory")]
    RegionAlreadyMapped,
    #[error("the owner's device window has no room left")]
    WindowExhausted,
    #[error("the request exceeds the device's pinned-memory budget")]
    BudgetExhausted,
    #[error("the alignment is not a power of two")]
    BadAlignment,
    #[error("a buffer the device cannot reach was allocated for it")]
    Unreachable,
    #[error("the address space refused the mapping: {0}")]
    AddressSpace(#[from] helios_hal::vmm::AddressSpaceError),
}

/// One device instance, whole: the physical ranges it answers on, the
/// interrupts it raises, what it may reach when it masters the bus, and
/// the confinement it sits in.
///
/// A grant is built by backend discovery and is immutable afterwards.
/// It says nothing about who owns the device — that is the registry's
/// business — and nothing about where the regions end up mapped, which
/// belongs to the owner's lease.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceGrant {
    name: DeviceName,
    regions: ArrayVec<DeviceRegion, MAX_GRANT_REGIONS>,
    interrupts: ArrayVec<GrantInterrupt, MAX_GRANT_INTERRUPTS>,
    dma: DmaBudget,
    confinement: Option<IommuDomain>,
}

impl DeviceGrant {
    /// A grant over the device `name`, with no regions or interrupts yet.
    pub fn new(name: DeviceName, dma: DmaBudget) -> Self {
        Self {
            name,
            regions: ArrayVec::new(),
            interrupts: ArrayVec::new(),
            dma,
            confinement: None,
        }
    }

    /// Add one of the device's physical ranges.
    ///
    /// A region that does not start and end on a frame boundary is
    /// refused: the page it shares with its neighbour would carry the
    /// neighbour's registers into the owner's memory, and the neighbour
    /// may be a device nobody granted away.
    pub fn with_region(mut self, region: DeviceRegion) -> Result<Self, GrantError> {
        if !region.is_frame_aligned() {
            return Err(GrantError::RegionNotFrameAligned);
        }
        self.regions
            .try_push(region)
            .map_err(|_| GrantError::TooManyRegions)?;
        Ok(self)
    }

    /// Add one of the device's interrupt sources.
    pub fn with_interrupt(mut self, interrupt: GrantInterrupt) -> Result<Self, GrantError> {
        self.interrupts
            .try_push(interrupt)
            .map_err(|_| GrantError::TooManyInterrupts)?;
        Ok(self)
    }

    /// Record that the platform confines this device behind a
    /// translation unit.
    pub fn confined_to(mut self, confinement: IommuDomain) -> Self {
        self.confinement = Some(confinement);
        self
    }

    pub fn name(&self) -> &DeviceName {
        &self.name
    }

    pub fn regions(&self) -> &[DeviceRegion] {
        &self.regions
    }

    pub fn interrupts(&self) -> &[GrantInterrupt] {
        &self.interrupts
    }

    pub const fn dma(&self) -> DmaBudget {
        self.dma
    }

    /// The confinement the device sits in, when the platform has a
    /// translation unit. `None` means the device reaches all of memory
    /// and its owner is isolated from the device's faults but not from
    /// its bus traffic.
    pub const fn confinement(&self) -> Option<IommuDomain> {
        self.confinement
    }

    /// Bytes of the owner's memory the regions need, once each has been
    /// rounded to whole frames.
    pub fn region_bytes(&self) -> u64 {
        self.regions
            .iter()
            .map(|region| (region.frame_count() as u64) * (helios_hal::pmm::PhysFrame::SIZE as u64))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeviceGrant, DeviceName, DmaBudget, GrantError, GrantInterrupt};
    use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability};
    use helios_hal::iommu::{DmaTranslation, PhysicalRange};

    fn budget() -> DmaBudget {
        DmaBudget {
            capability: DmaCapability {
                address_bits: 64,
                coherent: true,
                translation: DmaTranslation::direct(),
            },
            byte_budget: 1 << 20,
        }
    }

    #[test]
    fn a_region_that_shares_a_frame_with_its_neighbour_is_refused() {
        let grant = DeviceGrant::new(
            DeviceName::new("pci:0000:00:04.0").expect("short"),
            budget(),
        );

        let error = grant
            .with_region(DeviceRegion::new(
                PhysicalRange::new(0x4000_0800, 0x800),
                DeviceRegionAttributes::REGISTERS,
            ))
            .expect_err("a straddling region cannot be granted on its own");

        assert_eq!(error, GrantError::RegionNotFrameAligned);
    }

    #[test]
    fn region_bytes_counts_whole_frames() {
        let grant = DeviceGrant::new(DeviceName::new("test:device").expect("short"), budget())
            .with_region(DeviceRegion::new(
                PhysicalRange::new(0x4000_0000, 0x1000),
                DeviceRegionAttributes::REGISTERS,
            ))
            .expect("first region fits")
            .with_region(DeviceRegion::new(
                PhysicalRange::new(0x4001_0000, 0x4000),
                DeviceRegionAttributes::PREFETCHABLE_MEMORY,
            ))
            .expect("second region fits");

        assert_eq!(grant.region_bytes(), 0x5000);
        assert_eq!(grant.regions().len(), 2);
    }

    #[test]
    fn a_name_longer_than_the_registry_stores_is_refused() {
        let long = "x".repeat(super::MAX_DEVICE_NAME + 1);

        assert_eq!(DeviceName::new(&long), Err(GrantError::NameTooLong));
    }

    #[test]
    fn a_grant_holds_no_more_interrupts_than_it_can_route() {
        let mut grant = DeviceGrant::new(DeviceName::new("test:device").expect("short"), budget());
        for source in 0..super::MAX_GRANT_INTERRUPTS as u32 {
            grant = grant
                .with_interrupt(GrantInterrupt::new(source))
                .expect("interrupts up to the capacity fit");
        }

        assert_eq!(
            grant.with_interrupt(GrantInterrupt::new(99)).map(|_| ()),
            Err(GrantError::TooManyInterrupts)
        );
    }
}
