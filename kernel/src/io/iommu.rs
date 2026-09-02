//! IOMMU domain management.
//!
//! The hardware contract lives in `helios_hal::iommu`; the backends find
//! the translation unit and the endpoint identities their platform
//! publishes. What is left — deciding how many domains exist, where each
//! one's I/O virtual address space starts, and what it maps — is
//! hardware-independent, so it lives here.
//!
//! Helios gives every protected device a domain of its own, and every
//! domain a slot of the I/O virtual address space of its own. A device
//! therefore reaches nothing but its own slot: an address belonging to
//! another device's domain resolves to nothing at all, and the unit
//! faults instead of letting the access through. The slots start above
//! every doorbell the platform identity-maps, so a stray physical
//! address is never accidentally valid.
//!
//! Concurrency contract: domains are built on the bootstrap processor
//! while devices are brought up, before interrupts are enabled. The
//! [`DmaTranslation`] each device is left with is an immutable value
//! every processor reads without a lock, and [`IommuReport`] carries
//! only atomics.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use helios_hal::iommu::{
    DmaAccess, DmaTranslation, DmaWindow, DomainId, EndpointId, IoVirtAddr, Iommu, IommuError,
    IommuGeometry, MAX_DMA_WINDOWS, PhysicalRange,
};

/// How many devices one translation unit can confine.
///
/// A helios platform exposes a network device, a host share, an entropy
/// source and its disks; eight covers every machine the kernel boots on
/// with room for the plugin-hosted drivers to come.
pub const MAX_IOMMU_ENDPOINTS: usize = 8;

/// What one confined device's domain covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IommuEndpointStats {
    /// The endpoint identity the platform gave the device.
    pub endpoint: u32,
    /// The domain the device was attached to.
    pub domain: u32,
    /// Physical bytes the domain maps, doorbells included.
    pub mapped_bytes: u64,
}

/// A coherent view of what the platform's translation unit is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IommuStats {
    /// The smallest page the unit maps.
    pub granule_bytes: u64,
    /// Whether endpoints attached to no domain still reach memory.
    /// A confined device is translated either way.
    pub global_bypass: bool,
    /// Translation faults the unit has reported since boot.
    pub faults: u64,
    endpoints: [IommuEndpointStats; MAX_IOMMU_ENDPOINTS],
    endpoint_count: usize,
}

impl IommuStats {
    /// One entry per confined device.
    pub fn endpoints(&self) -> &[IommuEndpointStats] {
        &self.endpoints[..self.endpoint_count]
    }
}

/// The live IOMMU observation the kernel publishes.
///
/// The topology is fixed once the devices are up; only the fault counter
/// moves, and the backend's interrupt handler is what moves it.
#[derive(Debug)]
pub struct IommuReport {
    granule_bytes: u64,
    global_bypass: bool,
    endpoints: [IommuEndpointStats; MAX_IOMMU_ENDPOINTS],
    endpoint_count: usize,
    faults: AtomicU64,
}

impl IommuReport {
    /// Publishes the total number of faults the unit has reported.
    pub fn record_faults(&self, total: u64) {
        self.faults.store(total, Ordering::Relaxed);
    }

    /// The current observation.
    pub fn snapshot(&self) -> IommuStats {
        IommuStats {
            granule_bytes: self.granule_bytes,
            global_bypass: self.global_bypass,
            faults: self.faults.load(Ordering::Relaxed),
            endpoints: self.endpoints,
            endpoint_count: self.endpoint_count,
        }
    }
}

/// Builds and owns the translation domains of one IOMMU.
pub struct IommuDomains<I: Iommu> {
    iommu: I,
    geometry: IommuGeometry,
    /// The physical memory a confined device is allowed to reach,
    /// granule-aligned.
    memory: [Option<PhysicalRange>; MAX_DMA_WINDOWS],
    /// Ranges every domain maps at their own physical address because
    /// the device cannot be told to issue anything else for them —
    /// interrupt doorbells.
    doorbells: [Option<PhysicalRange>; MAX_DMA_WINDOWS],
    /// I/O virtual bytes one domain occupies.
    span: u64,
    /// Where the first domain's slot starts.
    first_base: u64,
    endpoints: [IommuEndpointStats; MAX_IOMMU_ENDPOINTS],
    endpoint_count: usize,
}

impl<I: Iommu> IommuDomains<I> {
    /// Prepares the domain layout for `iommu`.
    ///
    /// `memory` is the physical memory a confined device may reach and
    /// `doorbells` the ranges it has to keep reaching at their own
    /// address. Both are rounded outwards to the unit's granule, so a
    /// range that shares a page with something else brings that page
    /// along; the caller passes whole memory regions, not buffers.
    pub fn new(
        iommu: I,
        memory: &[PhysicalRange],
        doorbells: &[PhysicalRange],
    ) -> Result<Self, IommuError> {
        let geometry = iommu.geometry();
        let memory = align_ranges(memory, &geometry)?;
        let doorbells = align_ranges(doorbells, &geometry)?;
        if memory.iter().flatten().next().is_none() {
            return Err(IommuError::Invalid);
        }

        let span = memory
            .iter()
            .flatten()
            .try_fold(0_u64, |total, range| total.checked_add(range.bytes))
            .ok_or(IommuError::OutOfRange)?;
        // The domain slots start above every identity-mapped doorbell so
        // a domain address can never collide with one.
        let doorbell_end = doorbells
            .iter()
            .flatten()
            .map(|range| range.last())
            .max()
            .map(|last| last.checked_add(1).ok_or(IommuError::OutOfRange))
            .transpose()?
            .unwrap_or(0);
        let first_base = geometry.align_up(doorbell_end.max(*geometry.input_range.start()));

        Ok(Self {
            iommu,
            geometry,
            memory,
            doorbells,
            span,
            first_base,
            endpoints: [IommuEndpointStats::default(); MAX_IOMMU_ENDPOINTS],
            endpoint_count: 0,
        })
    }

    /// The translation unit these domains are built on.
    pub fn iommu(&self) -> &I {
        &self.iommu
    }

    /// Puts `endpoint` in a domain of its own and maps that domain's
    /// memory, returning the translation its driver publishes addresses
    /// through.
    pub fn confine(&mut self, endpoint: EndpointId) -> Result<DmaTranslation, IommuError> {
        let index = self.endpoint_count;
        if index >= MAX_IOMMU_ENDPOINTS {
            return Err(IommuError::OutOfResources);
        }
        let domain_id = u32::try_from(index)
            .ok()
            .and_then(|index| self.geometry.domain_range.start().checked_add(index))
            .filter(|domain| self.geometry.domain_range.contains(domain))
            .ok_or(IommuError::OutOfResources)?;
        let domain = DomainId::new(domain_id);

        let base = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_mul(self.span))
            .and_then(|offset| self.first_base.checked_add(offset))
            .ok_or(IommuError::OutOfRange)?;
        let last = base
            .checked_add(self.span - 1)
            .ok_or(IommuError::OutOfRange)?;
        if !self.geometry.input_range.contains(&last) {
            return Err(IommuError::OutOfRange);
        }

        self.iommu.attach(domain, endpoint)?;

        let mut translation = DmaTranslation::confined();
        let mut iova = base;
        for range in self.memory.iter().flatten() {
            self.iommu.map(
                domain,
                IoVirtAddr::new(iova),
                range.start,
                range.bytes,
                DmaAccess::READ | DmaAccess::WRITE,
            )?;
            translation = translation.with_window(DmaWindow {
                physical_start: range.start,
                bytes: range.bytes,
                iova_start: IoVirtAddr::new(iova),
            })?;
            iova += range.bytes;
        }
        for range in self.doorbells.iter().flatten() {
            // A doorbell is written by the device at an address the
            // interrupt controller fixed, so the only mapping that can
            // work is the identity one.
            self.iommu.map(
                domain,
                IoVirtAddr::new(range.start),
                range.start,
                range.bytes,
                DmaAccess::WRITE | DmaAccess::MMIO,
            )?;
            translation = translation.with_window(DmaWindow {
                physical_start: range.start,
                bytes: range.bytes,
                iova_start: IoVirtAddr::new(range.start),
            })?;
        }

        self.endpoints[index] = IommuEndpointStats {
            endpoint: endpoint.get(),
            domain: domain_id,
            mapped_bytes: translation.mapped_bytes(),
        };
        self.endpoint_count = index + 1;
        tracing::info!(
            endpoint = %endpoint,
            domain = domain_id,
            iova_base = base,
            mapped_bytes = translation.mapped_bytes(),
            granule = self.geometry.granule(),
            "virtio device confined to its own IOMMU domain"
        );
        Ok(translation)
    }

    /// The observation the kernel publishes for these domains.
    ///
    /// `global_bypass` says whether endpoints outside every domain still
    /// reach memory; the confined ones are translated regardless.
    pub fn report(&self, global_bypass: bool) -> Arc<IommuReport> {
        Arc::new(IommuReport {
            granule_bytes: self.geometry.granule(),
            global_bypass,
            endpoints: self.endpoints,
            endpoint_count: self.endpoint_count,
            faults: AtomicU64::new(0),
        })
    }
}

fn align_ranges(
    ranges: &[PhysicalRange],
    geometry: &IommuGeometry,
) -> Result<[Option<PhysicalRange>; MAX_DMA_WINDOWS], IommuError> {
    let mut aligned = [const { None }; MAX_DMA_WINDOWS];
    for (slot, range) in ranges.iter().enumerate() {
        if slot >= MAX_DMA_WINDOWS {
            return Err(IommuError::TooManyWindows);
        }
        aligned[slot] = Some(range.aligned_to(geometry));
    }
    Ok(aligned)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use helios_hal::iommu::{
        DmaAccess, DomainId, EndpointId, IoVirtAddr, Iommu, IommuError, IommuGeometry,
        PhysicalRange,
    };
    use spin::Mutex;

    use super::{IommuDomains, MAX_IOMMU_ENDPOINTS};

    /// `-(4 * KiB)`: a unit that maps from 4 KiB pages upwards.
    const PAGE_SIZE_MASK: u64 = 0xffff_ffff_ffff_f000;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Command {
        Attach {
            domain: u32,
            endpoint: u32,
        },
        Map {
            domain: u32,
            iova: u64,
            physical: u64,
            bytes: u64,
            access: DmaAccess,
        },
    }

    /// A unit that records what the domain builder asked it to do.
    struct RecordingIommu {
        geometry: IommuGeometry,
        commands: Mutex<Vec<Command>>,
    }

    impl RecordingIommu {
        fn new(domain_last: u32) -> Self {
            Self {
                geometry: IommuGeometry {
                    page_size_mask: PAGE_SIZE_MASK,
                    input_range: 0..=u64::MAX,
                    domain_range: 0..=domain_last,
                },
                commands: Mutex::new(Vec::new()),
            }
        }

        fn commands(&self) -> Vec<Command> {
            self.commands.lock().clone()
        }
    }

    impl Iommu for RecordingIommu {
        fn geometry(&self) -> IommuGeometry {
            self.geometry.clone()
        }

        fn attach(&self, domain: DomainId, endpoint: EndpointId) -> Result<(), IommuError> {
            self.commands.lock().push(Command::Attach {
                domain: domain.get(),
                endpoint: endpoint.get(),
            });
            Ok(())
        }

        fn detach(&self, _domain: DomainId, _endpoint: EndpointId) -> Result<(), IommuError> {
            Ok(())
        }

        fn map(
            &self,
            domain: DomainId,
            iova: IoVirtAddr,
            physical: u64,
            bytes: u64,
            access: DmaAccess,
        ) -> Result<(), IommuError> {
            self.commands.lock().push(Command::Map {
                domain: domain.get(),
                iova: iova.get(),
                physical,
                bytes,
                access,
            });
            Ok(())
        }

        fn unmap(
            &self,
            _domain: DomainId,
            _iova: IoVirtAddr,
            _bytes: u64,
        ) -> Result<(), IommuError> {
            Ok(())
        }
    }

    fn memory() -> [PhysicalRange; 1] {
        [PhysicalRange::new(0x4000_0000, 0x1000_0000)]
    }

    fn doorbells() -> [PhysicalRange; 1] {
        [PhysicalRange::new(0xfee0_0000, 0x10_0000)]
    }

    #[test]
    fn each_device_gets_its_own_domain_and_its_own_slot() {
        let mut domains = IommuDomains::new(RecordingIommu::new(u32::MAX), &memory(), &doorbells())
            .expect("the layout is buildable");

        let first = domains
            .confine(EndpointId::new(0x18))
            .expect("the first device is confined");
        let second = domains
            .confine(EndpointId::new(0x20))
            .expect("the second device is confined");

        // The slots start above the doorbell window and do not overlap.
        assert_eq!(first.device_address(0x4000_0000), Ok(0xfef0_0000));
        assert_eq!(second.device_address(0x4000_0000), Ok(0x1_0ef0_0000));
        // Neither device can name the other's memory through its own
        // address space: the addresses simply are not the same.
        assert_ne!(
            first.device_address(0x4000_0000),
            second.device_address(0x4000_0000)
        );
        // Both keep their doorbell at its physical address.
        assert_eq!(first.device_address(0xfee0_0000), Ok(0xfee0_0000));
        assert_eq!(second.device_address(0xfee0_0000), Ok(0xfee0_0000));
    }

    #[test]
    fn confining_a_device_attaches_it_and_maps_its_memory() {
        let mut domains = IommuDomains::new(RecordingIommu::new(u32::MAX), &memory(), &doorbells())
            .expect("the layout is buildable");
        domains
            .confine(EndpointId::new(0x18))
            .expect("the device is confined");

        assert_eq!(
            domains.iommu().commands(),
            alloc::vec![
                Command::Attach {
                    domain: 0,
                    endpoint: 0x18
                },
                Command::Map {
                    domain: 0,
                    iova: 0xfef0_0000,
                    physical: 0x4000_0000,
                    bytes: 0x1000_0000,
                    access: DmaAccess::READ | DmaAccess::WRITE,
                },
                Command::Map {
                    domain: 0,
                    iova: 0xfee0_0000,
                    physical: 0xfee0_0000,
                    bytes: 0x10_0000,
                    access: DmaAccess::WRITE | DmaAccess::MMIO,
                },
            ]
        );
    }

    /// A memory region that does not sit on a page boundary still has to
    /// be mappable, so it is rounded outwards rather than refused.
    #[test]
    fn memory_regions_are_rounded_out_to_the_granule() {
        let mut domains = IommuDomains::new(
            RecordingIommu::new(u32::MAX),
            &[PhysicalRange::new(0x4000_0800, 0x1800)],
            &[],
        )
        .expect("the layout is buildable");
        domains
            .confine(EndpointId::new(0x18))
            .expect("the device is confined");

        assert_eq!(
            domains.iommu().commands()[1],
            Command::Map {
                domain: 0,
                iova: 0,
                physical: 0x4000_0000,
                bytes: 0x2000,
                access: DmaAccess::READ | DmaAccess::WRITE,
            }
        );
    }

    #[test]
    fn a_unit_with_too_few_domains_refuses_the_next_device() {
        let mut domains = IommuDomains::new(RecordingIommu::new(0), &memory(), &[])
            .expect("the layout is buildable");

        domains
            .confine(EndpointId::new(0x18))
            .expect("the only domain is available");
        assert_eq!(
            domains.confine(EndpointId::new(0x20)),
            Err(IommuError::OutOfResources)
        );
    }

    #[test]
    fn the_report_names_every_confined_device() {
        let mut domains = IommuDomains::new(RecordingIommu::new(u32::MAX), &memory(), &doorbells())
            .expect("the layout is buildable");
        domains
            .confine(EndpointId::new(0x18))
            .expect("the device is confined");
        domains
            .confine(EndpointId::new(0x20))
            .expect("the device is confined");

        let report = domains.report(true);
        report.record_faults(3);
        let stats = report.snapshot();

        assert_eq!(stats.granule_bytes, 4096);
        assert!(stats.global_bypass);
        assert_eq!(stats.faults, 3);
        assert_eq!(stats.endpoints().len(), 2);
        assert_eq!(stats.endpoints()[0].endpoint, 0x18);
        assert_eq!(stats.endpoints()[0].domain, 0);
        assert_eq!(stats.endpoints()[0].mapped_bytes, 0x1000_0000 + 0x10_0000);
        assert_eq!(stats.endpoints()[1].domain, 1);
    }

    #[test]
    fn a_unit_with_no_memory_to_map_is_refused() {
        assert_eq!(
            IommuDomains::new(RecordingIommu::new(u32::MAX), &[], &[]).err(),
            Some(IommuError::Invalid)
        );
    }

    #[test]
    fn more_devices_than_the_kernel_tracks_are_refused() {
        let mut domains = IommuDomains::new(RecordingIommu::new(u32::MAX), &memory(), &[])
            .expect("the layout is buildable");
        for index in 0..MAX_IOMMU_ENDPOINTS {
            domains
                .confine(EndpointId::new(index as u32))
                .expect("every tracked endpoint is confined");
        }

        assert_eq!(
            domains.confine(EndpointId::new(0xffff)),
            Err(IommuError::OutOfResources)
        );
    }
}
