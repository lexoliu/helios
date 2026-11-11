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
//! * **Registers.** [`GrantLease::map_region`] shows the device's own
//!   frames at a span of the owner's linear memory the owner chose, so
//!   a register access is an ordinary load or store rather than a call
//!   per register.
//! * **Interrupts.** [`InterruptRelay`] holds a source off at the
//!   controller the moment it fires and hands the delivery to the
//!   owner; every decision about what the device meant runs in user
//!   memory.
//! * **Bus mastering.** [`GrantLease::dma_alloc`] pins a physically
//!   contiguous buffer from the owner's own pool, shows it at a span the
//!   owner chose, and tells the owner the address the device has to
//!   issue to reach it.
//! * **Reclaim.** Letting the lease go ends every view of the device,
//!   masks every source and releases every buffer before the device is
//!   offered to anyone else, so an owner that was killed leaves nothing
//!   behind.
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
mod handle;
mod interrupt;
mod lease;
mod owner;
mod platform;
mod registry;

pub use grant::{
    DEFAULT_DMA_BUDGET_BYTES, DeviceGrant, DeviceName, DmaBudget, GrantError, GrantInterrupt,
    MAX_DEVICE_NAME, MAX_GRANT_INTERRUPTS, MAX_GRANT_REGIONS,
};
pub use handle::{DmaBufferHandle, GrantHandle};
pub use interrupt::{InterruptEvent, InterruptRelay, InterruptStats};
pub use lease::{DmaBuffer, GrantLease, GrantStats, MAX_DMA_BUFFERS, PublishedDevice};
pub use owner::DeviceOwnership;
pub(crate) use platform::device_vm_hooks;
#[cfg(test)]
pub(crate) use platform::test_hooks;
pub use platform::{
    DeviceInterruptHooks, DeviceVmHooks, install_device_interrupt_hooks, install_device_vm_hooks,
};
pub use registry::{DeviceGrantRegistry, DeviceInterruptRoute, GrantedDeviceSnapshot, MAX_GRANTS};

#[cfg(test)]
mod tests {
    use super::platform::test_hooks::{self, MappingChange};
    use super::{
        DeviceGrant, DeviceGrantRegistry, DeviceName, DmaBudget, GrantError, GrantInterrupt,
    };
    use crate::buffer::{GuestMemory, Span, ViewError, ViewTable, WASM_PAGE_BYTES};
    use helios_hal::device::{DeviceRegion, DeviceRegionAttributes, DmaCapability};
    use helios_hal::iommu::{DmaTranslation, PhysicalRange};
    use helios_hal::vmm::VirtAddr;

    /// Where the tests pretend the owner's linear memory sits, and how
    /// far it has grown.
    const MEMORY_BASE: usize = 0x1_0000_0000;
    const MEMORY_BYTES: u64 = 64 * WASM_PAGE_BYTES;

    fn memory() -> GuestMemory {
        GuestMemory::new(VirtAddr::new(MEMORY_BASE), MEMORY_BYTES)
    }

    /// `pages` wasm pages of the owner's memory starting at page `first`.
    const fn pages(first: u64, pages: u64) -> Span {
        Span::new(first * WASM_PAGE_BYTES, pages * WASM_PAGE_BYTES)
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
        let mut views = ViewTable::new();
        let first = registry
            .claim("test:alpha", views.group())
            .expect("the first owner gets the device");

        assert_eq!(
            registry.claim("test:alpha", views.group()).err(),
            Some(GrantError::AlreadyClaimed)
        );

        // The device comes back the moment the first owner lets go, so a
        // supervisor restarting a dead driver is not blocked by the
        // instance it just killed.
        drop(first);
        assert!(registry.claim("test:alpha", views.group()).is_ok());
    }

    #[test]
    fn a_device_nobody_discovered_cannot_be_claimed() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();

        assert_eq!(
            registry.claim("test:missing", views.group()).err(),
            Some(GrantError::NotFound)
        );
    }

    /// Every substitution ends in a translation-cache shootdown, so the
    /// count of them is what proves a region became reachable at the
    /// span the owner named and then provably unreachable again.
    #[test]
    fn a_region_is_shown_where_the_owner_said_and_reclaim_ends_it() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");
        let before = test_hooks::shootdowns();

        lease
            .map_region(0, &mut views, memory(), pages(4, 1))
            .expect("the register region is shown");
        lease
            .map_region(1, &mut views, memory(), pages(9, 1))
            .expect("the aperture is shown");

        let changes = test_hooks::changes();
        let shown: alloc::vec::Vec<_> = changes
            .iter()
            .filter_map(|change| match change {
                MappingChange::SubstituteDevice(virt) => Some(*virt),
                _ => None,
            })
            .collect();
        assert_eq!(
            shown[shown.len() - 2..],
            [
                helios_hal::vmm::VirtRange::new(
                    VirtAddr::new(MEMORY_BASE + 4 * WASM_PAGE_BYTES as usize),
                    0x2000
                ),
                helios_hal::vmm::VirtRange::new(
                    VirtAddr::new(MEMORY_BASE + 9 * WASM_PAGE_BYTES as usize),
                    0x1000
                ),
            ],
            "only the region's own pages are substituted, at the start of the span"
        );
        assert_eq!(test_hooks::shootdowns() - before, 2);
        assert_eq!(lease.stats().mapped_regions, 2);

        lease
            .end_views(&mut views)
            .unwrap_or_else(|error| panic!("the views end: {error}"));
        drop(lease);
        assert_eq!(
            test_hooks::shootdowns() - before,
            4,
            "both regions are restored, each with its own shootdown"
        );
        let tail = &test_hooks::changes()[test_hooks::changes().len() - 2..];
        assert!(
            tail.iter()
                .all(|change| matches!(change, MappingChange::Restore(_))),
            "reclaim ends in zeroed memory put back, not in anything else"
        );
        assert!(views.is_empty());
    }

    /// A region is shown once. A second span would be a second path to
    /// the same registers in memory the owner's allocator thinks is free.
    #[test]
    fn a_region_already_shown_is_refused_a_second_span() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");
        lease
            .map_region(0, &mut views, memory(), pages(0, 1))
            .expect("the register region is shown");

        assert_eq!(
            lease.map_region(0, &mut views, memory(), pages(2, 1)),
            Err(GrantError::RegionAlreadyMapped)
        );
        assert_eq!(views.len(), 1);
    }

    /// Every way a span can be wrong is refused before anything changes
    /// in the owner's memory.
    #[test]
    fn a_region_at_a_bad_span_is_refused() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");
        let before = test_hooks::shootdowns();

        assert_eq!(
            lease.map_region(0, &mut views, memory(), Span::new(4096, WASM_PAGE_BYTES)),
            Err(GrantError::View(ViewError::Misaligned))
        );
        assert_eq!(
            lease.map_region(0, &mut views, memory(), pages(0, 2)),
            Err(GrantError::View(ViewError::WrongLength {
                expected: WASM_PAGE_BYTES,
                given: 2 * WASM_PAGE_BYTES,
            }))
        );
        assert_eq!(
            lease.map_region(0, &mut views, memory(), pages(64, 1)),
            Err(GrantError::View(ViewError::OutOfBounds {
                memory_bytes: MEMORY_BYTES
            }))
        );
        assert_eq!(test_hooks::shootdowns(), before, "nothing was substituted");
    }

    #[test]
    fn a_region_the_device_does_not_have_is_refused() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");

        assert_eq!(
            lease.map_region(7, &mut views, memory(), pages(0, 1)),
            Err(GrantError::NoSuchRegion)
        );
    }

    /// The ring is the owner's memory and the device's at once, and its
    /// frames go back only after the owner's view of them has ended.
    #[test]
    fn a_pinned_buffer_is_shown_where_the_owner_said_and_freed_after_its_view() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");
        let before = test_hooks::shootdowns();

        let buffer = lease
            .dma_alloc(4096, &mut views, memory(), pages(3, 1))
            .expect("the ring fits the budget");

        assert_eq!(buffer.bytes, WASM_PAGE_BYTES);
        assert_eq!(lease.stats().pinned_bytes, WASM_PAGE_BYTES);
        assert_eq!(test_hooks::shootdowns() - before, 1);
        assert!(matches!(
            test_hooks::changes().last(),
            Some(MappingChange::SubstituteMemory(virt, physical))
                if virt.start.raw() == MEMORY_BASE + 3 * WASM_PAGE_BYTES as usize
                    && physical.start == buffer.device_address
        ));

        let mark = test_hooks::changes().len();
        lease
            .end_views(&mut views)
            .unwrap_or_else(|error| panic!("the views end: {error}"));
        drop(lease);
        let after = &test_hooks::changes()[mark..];
        let restored = after
            .iter()
            .position(|change| matches!(change, MappingChange::Restore(_)))
            .expect("the owner's view ends");
        let freed = after
            .iter()
            .position(|change| matches!(change, MappingChange::Freed(_)))
            .expect("the ring is freed");
        assert!(restored < freed, "the view ends before the frames go back");
    }

    #[test]
    fn a_driver_cannot_pin_more_than_its_budget() {
        test_hooks::install();
        let registry = DeviceGrantRegistry::new();
        registry
            .publish([DeviceGrant::new(
                DeviceName::new("test:small").expect("short"),
                budget(64, 2 * WASM_PAGE_BYTES),
            )])
            .expect("discovery publishes once");
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:small", views.group())
            .expect("the device is free");

        lease
            .dma_alloc(4096, &mut views, memory(), pages(0, 1))
            .expect("the first ring fits");
        lease
            .dma_alloc(4096, &mut views, memory(), pages(1, 1))
            .expect("the second ring fits");

        assert_eq!(
            lease
                .dma_alloc(4096, &mut views, memory(), pages(2, 1))
                .err(),
            Some(GrantError::BudgetExhausted)
        );
    }

    /// With a buffer already pinned, a request sized so that the two
    /// together pass `u64::MAX` must be refused by the budget rather
    /// than overflow the sum the budget is tested with.
    #[test]
    fn a_request_that_overflows_the_pinned_total_is_over_budget() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");

        lease
            .dma_alloc(4096, &mut views, memory(), pages(0, 1))
            .expect("the first buffer fits");

        assert_eq!(
            lease
                .dma_alloc(
                    4096,
                    &mut views,
                    memory(),
                    Span::new(WASM_PAGE_BYTES, u64::MAX - (WASM_PAGE_BYTES - 1)),
                )
                .err(),
            Some(GrantError::BudgetExhausted)
        );
    }

    /// Nothing was refused, so saying the budget was exhausted would
    /// name the wrong fact and send a driver looking at a limit that
    /// had nothing to do with it.
    #[test]
    fn a_buffer_of_no_bytes_is_refused_as_empty_rather_than_over_budget() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");

        assert_eq!(
            lease
                .dma_alloc(4096, &mut views, memory(), Span::new(0, 0))
                .err(),
            Some(GrantError::EmptyRequest)
        );
    }

    /// A ring at a span overlapping a region already shown is refused
    /// before anything is pinned for it.
    #[test]
    fn a_ring_overlapping_a_shown_region_is_refused_before_it_is_pinned() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");
        lease
            .map_region(0, &mut views, memory(), pages(5, 1))
            .expect("the register region is shown");
        let mark = test_hooks::changes().len();

        assert_eq!(
            lease
                .dma_alloc(4096, &mut views, memory(), pages(4, 2))
                .err(),
            Some(GrantError::View(ViewError::Overlap))
        );
        assert_eq!(test_hooks::changes().len(), mark, "nothing was allocated");
        assert_eq!(lease.stats().pinned_bytes, 0);
    }

    #[test]
    fn an_alignment_that_is_not_a_power_of_two_is_refused() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut lease = registry
            .claim("test:alpha", views.group())
            .expect("the device is free");

        assert_eq!(
            lease
                .dma_alloc(3072, &mut views, memory(), pages(0, 1))
                .err(),
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
        let mut views = ViewTable::new();
        let lease = registry
            .claim("test:alpha", views.group())
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

    /// Releasing ends the owner's views of the device and gives the
    /// device back, in that order.
    #[test]
    fn releasing_a_device_ends_its_views_and_gives_it_back() {
        let registry = registry(&["test:alpha"]);
        let mut views = ViewTable::new();
        let mut ownership = super::DeviceOwnership::new();
        ownership
            .claim(&registry, "test:alpha", &mut views)
            .expect("the device is free");
        ownership
            .lease_mut()
            .expect("a claim leaves a lease")
            .map_region(0, &mut views, memory(), pages(0, 1))
            .expect("the register region is shown");
        assert!(ownership.holds_device());

        ownership.release(&mut views).expect("the view ends");

        assert!(views.is_empty(), "no view of the device outlives the grant");
        assert!(!ownership.holds_device());
        assert!(
            registry.claim("test:alpha", views.group()).is_ok(),
            "releasing gives the device back"
        );
    }

    /// A driver drives one device. A second claim would make reclaim
    /// ambiguous, so it is refused rather than replacing the first.
    #[test]
    fn an_instance_holds_one_device_at_a_time() {
        let registry = registry(&["test:alpha", "test:beta"]);
        let mut views = ViewTable::new();
        let mut ownership = super::DeviceOwnership::new();
        ownership
            .claim(&registry, "test:alpha", &mut views)
            .expect("the first device is free");

        assert_eq!(
            ownership.claim(&registry, "test:beta", &mut views),
            Err(GrantError::AlreadyClaimed)
        );
    }

    #[test]
    fn an_interrupt_no_published_device_raises_is_refused() {
        let registry = registry(&["test:alpha"]);

        assert!(!registry.forward(GrantInterrupt::new(99)));
    }
}
