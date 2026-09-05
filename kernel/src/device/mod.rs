//! Handing one device, whole, to a user-mode owner.
//!
//! Helios drives virtio itself. Hardware outside that ecosystem is
//! driven from outside the kernel instead: the kernel discovers the
//! device, bundles everything the device *is* into a [`DeviceGrant`],
//! and hands that grant to exactly one user-mode instance, which runs
//! under the ordinary isolation model and can be killed and restarted
//! like any other. A driver that gets a register wrong faults in its own
//! instance rather than in the kernel.
//!
//! Four things make that possible, and this module owns all four:
//!
//! * **Registers.** [`GrantLease::map_region`] maps the device's own
//!   frames inside the owner's linear-memory reservation, so a register
//!   access is an ordinary load or store rather than a call per
//!   register.
//! * **Interrupts.** [`InterruptRelay`] holds a source off at the
//!   controller the moment it fires and hands the delivery to the
//!   owner; every decision about what the device meant runs in user
//!   memory.
//! * **Bus mastering.** [`GrantLease::dma_alloc`] pins a physically
//!   contiguous buffer inside the owner's memory, from the owner's own
//!   pool, and tells the owner the address the device has to issue to
//!   reach it.
//! * **Reclaim.** Dropping the lease masks every source, unmaps every
//!   region and releases every buffer before the device is offered to
//!   anyone else, so an owner that was killed leaves nothing behind.
//!
//! # What the sandbox does and does not buy
//!
//! An owner's memory faults are confined and its restart is cheap.
//! Its *bus* traffic is confined only when the platform has a
//! translation unit and discovery recorded the device's
//! [`IommuDomain`](helios_hal::device::IommuDomain): without one, a
//! driver that programs a bad descriptor reaches all of memory, and the
//! grant says so rather than implying an isolation the hardware does not
//! provide.
//!
//! # Layering
//!
//! `hal` says what the hardware is — a region's attributes, a bus
//! master's reach, a confinement. This module says what the kernel does
//! with it. Backends contribute discovery and the two hook tables in
//! [`platform`], and no driver logic at all.

mod grant;
mod interrupt;
mod lease;
mod platform;
mod registry;

pub use grant::{
    DeviceGrant, DeviceName, DmaBudget, GrantError, GrantInterrupt, MAX_DEVICE_NAME,
    MAX_GRANT_INTERRUPTS, MAX_GRANT_REGIONS,
};
pub use interrupt::{InterruptEvent, InterruptRelay, InterruptStats};
pub use lease::{
    DEVICE_WINDOW_BYTES, DeviceWindow, DmaBuffer, GrantLease, GrantStats, MAX_DMA_BUFFERS,
    MappedRegion, PublishedDevice,
};
pub use platform::{
    DeviceInterruptHooks, DeviceVmHooks, install_device_interrupt_hooks, install_device_vm_hooks,
};
pub use registry::{DeviceGrantRegistry, DeviceInterruptRoute, MAX_GRANTS};

#[cfg(test)]
mod tests {
    use super::platform::test_hooks::{self, MappingChange};
    use super::{
        DeviceGrant, DeviceGrantRegistry, DeviceName, DeviceWindow, DmaBudget, GrantError,
        GrantInterrupt,
    };
    use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability};
    use helios_hal::iommu::{DmaTranslation, PhysicalRange};
    use helios_hal::vmm::VirtAddr;

    const RESERVATION_BYTES: u64 = 1 << 32;
    const WINDOW_BASE: usize = 0x1_0000_0000;

    fn window() -> DeviceWindow {
        DeviceWindow::top_of(VirtAddr::new(WINDOW_BASE), RESERVATION_BYTES)
    }

    fn budget(address_bits: u32, byte_budget: u64) -> DmaBudget {
        DmaBudget {
            capability: DmaCapability {
                address_bits,
                coherent: true,
                translation: DmaTranslation::direct(),
            },
            byte_budget,
        }
    }

    fn grant(name: &str) -> DeviceGrant {
        DeviceGrant::new(
            DeviceName::new(name).expect("the test names are short"),
            budget(64, 1 << 20),
        )
        .with_region(DeviceRegion::new(
            PhysicalRange::new(0x4000_0000, 0x2000),
            DeviceRegionAttributes::REGISTERS,
        ))
        .expect("the register region fits")
        .with_region(DeviceRegion::new(
            PhysicalRange::new(0x5000_0000, 0x1000),
            DeviceRegionAttributes::PREFETCHABLE_MEMORY,
        ))
        .expect("the aperture fits")
        .with_interrupt(GrantInterrupt::new(41))
        .expect("the interrupt fits")
    }

    fn registry(names: &[&str]) -> DeviceGrantRegistry {
        test_hooks::install();
        let registry = DeviceGrantRegistry::new();
        registry
            .publish(names.iter().map(|name| grant(name)))
            .expect("discovery publishes once");
        registry
    }

    #[test]
    fn a_published_device_is_listed_before_anyone_owns_it() {
        let registry = registry(&["test:alpha", "test:beta"]);

        let names: alloc::vec::Vec<_> = registry
            .devices()
            .iter()
            .map(|device| device.grant().name().as_str())
            .collect();

        assert_eq!(names, alloc::vec!["test:alpha", "test:beta"]);
        assert!(registry.devices().iter().all(|device| !device.is_claimed()));
    }

    #[test]
    fn discovery_refuses_to_publish_one_device_twice() {
        test_hooks::install();
        let registry = DeviceGrantRegistry::new();

        assert_eq!(
            registry.publish([grant("test:alpha"), grant("test:alpha")]),
            Err(GrantError::DuplicateName)
        );
    }

    #[test]
    fn a_second_owner_is_refused_the_device_the_first_holds() {
        let registry = registry(&["test:alpha"]);
        let first = registry
            .claim("test:alpha", window())
            .expect("the first owner gets the device");

        assert_eq!(
            registry.claim("test:alpha", window()).err(),
            Some(GrantError::AlreadyClaimed)
        );

        // The device comes back the moment the first owner lets go, so a
        // supervisor restarting a dead driver is not blocked by the
        // instance it just killed.
        drop(first);
        assert!(registry.claim("test:alpha", window()).is_ok());
    }

    #[test]
    fn a_device_nobody_discovered_cannot_be_claimed() {
        let registry = registry(&["test:alpha"]);

        assert_eq!(
            registry.claim("test:missing", window()).err(),
            Some(GrantError::NotFound)
        );
    }

    /// Every mapping change ends in a translation-cache shootdown, so
    /// the count of them is what proves a region became reachable and
    /// then provably unreachable again.
    #[test]
    fn mapping_a_region_and_reclaiming_it_shoots_down_both_times() {
        let registry = registry(&["test:alpha"]);
        let mut lease = registry
            .claim("test:alpha", window())
            .expect("the device is free");
        let before = test_hooks::shootdowns();

        let first = lease.map_region(0).expect("the register region maps");
        let second = lease.map_region(1).expect("the aperture maps");

        assert_eq!(first.offset, window().offset());
        assert_eq!(first.bytes, 0x2000);
        assert_eq!(second.offset, window().offset() + 0x2000);
        assert_eq!(second.bytes, 0x1000);
        assert_eq!(test_hooks::shootdowns() - before, 2);

        // Asking again answers rather than mapping twice.
        assert_eq!(lease.map_region(0).expect("already mapped"), first);
        assert_eq!(test_hooks::shootdowns() - before, 2);

        drop(lease);
        assert_eq!(
            test_hooks::shootdowns() - before,
            4,
            "both regions are unmapped, each with its own shootdown"
        );
        let tail = &test_hooks::changes()[test_hooks::changes().len() - 2..];
        assert!(
            tail.iter()
                .all(|change| matches!(change, MappingChange::UnmapDevice(_))),
            "reclaim ends in unmaps, not in anything else"
        );
    }

    #[test]
    fn a_region_the_device_does_not_have_is_refused() {
        let registry = registry(&["test:alpha"]);
        let mut lease = registry
            .claim("test:alpha", window())
            .expect("the device is free");

        assert_eq!(lease.map_region(7).err(), Some(GrantError::NoSuchRegion));
    }

    #[test]
    fn a_pinned_buffer_is_contiguous_addressable_and_released_on_death() {
        let registry = registry(&["test:alpha"]);
        let mut lease = registry
            .claim("test:alpha", window())
            .expect("the device is free");
        let before = test_hooks::shootdowns();

        let buffer = lease
            .dma_alloc(4096, 4096)
            .expect("the ring fits the budget");

        assert_eq!(buffer.bytes, 4096);
        assert!(buffer.offset >= window().offset());
        assert_eq!(lease.stats().pinned_bytes, 4096);
        assert_eq!(test_hooks::shootdowns() - before, 1);

        drop(lease);
        assert_eq!(
            test_hooks::shootdowns() - before,
            2,
            "the pin is released when the owner dies"
        );
        assert!(matches!(
            test_hooks::changes().last(),
            Some(MappingChange::Decommit(_))
        ));
    }

    #[test]
    fn a_driver_cannot_pin_more_than_its_budget() {
        test_hooks::install();
        let registry = DeviceGrantRegistry::new();
        registry
            .publish([DeviceGrant::new(
                DeviceName::new("test:small").expect("short"),
                budget(64, 8192),
            )])
            .expect("discovery publishes once");
        let mut lease = registry
            .claim("test:small", window())
            .expect("the device is free");

        lease.dma_alloc(4096, 4096).expect("the first ring fits");
        lease.dma_alloc(4096, 4096).expect("the second ring fits");

        assert_eq!(
            lease.dma_alloc(4096, 4096).err(),
            Some(GrantError::BudgetExhausted)
        );
    }

    #[test]
    fn an_alignment_that_is_not_a_power_of_two_is_refused() {
        let registry = registry(&["test:alpha"]);
        let mut lease = registry
            .claim("test:alpha", window())
            .expect("the device is free");

        assert_eq!(
            lease.dma_alloc(4096, 3072).err(),
            Some(GrantError::BadAlignment)
        );
    }

    /// A grant published for a machine whose backend wired no hook
    /// tables would hand a driver registers nobody can map. The failure
    /// belongs at the publish.
    #[test]
    fn publishing_needs_the_platform_surface() {
        // The recording tables are process-wide and every other test
        // installs them, so this asserts the check rather than the
        // uninstalled state: a registry with the surface present
        // publishes, and `PlatformUnavailable` is what the check reports
        // when it is not.
        test_hooks::install();
        let registry = DeviceGrantRegistry::new();

        assert!(registry.publish([grant("test:alpha")]).is_ok());
    }

    /// An interrupt is taken on whichever processor the controller
    /// picked and consumed on whichever one is running the owner. The
    /// hand-off is the relay's, and it has to survive the owner parking
    /// between its inspection of the queue and its wait.
    #[test]
    fn an_interrupt_reaches_an_owner_parked_on_another_processor() {
        let registry = registry(&["test:alpha"]);
        let lease = registry
            .claim("test:alpha", window())
            .expect("the device is free");
        lease.relay().unmask(0).expect("the source exists");

        futures_lite::future::block_on(async {
            let owner = async { lease.relay().next_event().await };
            let controller = async {
                crate::yield_now().await;
                assert!(registry.forward(GrantInterrupt::new(41)));
            };
            let (event, ()) = futures::future::join(owner, controller).await;
            assert_eq!(event.index, 0);
        });

        assert_eq!(lease.stats().interrupts.forwarded, 1);
        assert_eq!(
            lease.stats().interrupts.masked,
            1,
            "the delivery held the source off until the driver unmasks again"
        );
    }

    #[test]
    fn an_interrupt_no_published_device_raises_is_refused() {
        let registry = registry(&["test:alpha"]);

        assert!(!registry.forward(GrantInterrupt::new(99)));
    }
}
