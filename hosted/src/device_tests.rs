//! What a granted device does, driven through the hosted machine.
//!
//! These exercise the kernel's device path against a real address space
//! rather than a recording one: the views are host mappings, the pinned
//! buffers are host memory, and the alias a shown region creates is
//! provable by writing through one view and reading the other. What the
//! kernel's own tests assert about bookkeeping, these assert about the
//! memory underneath it.

use helios_hal::iommu::PhysicalRange;
use helios_hal::vmm::{
    AddressSpace, AddressSpaceError, PageFlags, Translation, VirtAddr, VirtRange,
};
use helios_kernel::{
    DeviceGrantClaimCap, DeviceGrantRegistry, DeviceName, GrantError, GrantInterrupt, GrantLease,
    GuestMemory, ProcessAuthority, ProcessAuthorityError, Span, ViewError, ViewTable,
    WASM_PAGE_BYTES,
};

use crate::device::{
    HOSTED_DEVICE_INTERRUPT, HOSTED_DEVICE_NAME, device_address_space, device_registers,
    hosted_device_grants, interrupt_controller_counts,
};
use crate::vmm::host_mapping_granule;

/// An owner's linear memory: room for the hosted device's whole DMA
/// budget (a megabyte) and a register file beside it.
const MEMORY_BYTES: u64 = 32 * WASM_PAGE_BYTES;

/// The span the hosted device's register file is shown at: its two host
/// pages, rounded up to one wasm page.
const REGISTERS_AT: Span = Span::new(0, WASM_PAGE_BYTES);

/// The span a ring is shown at, past the register file.
const RING_AT: Span = Span::new(WASM_PAGE_BYTES, WASM_PAGE_BYTES);

/// One owner's linear memory, reserved and committed as a wasm memory
/// is, and released when the test drops it.
struct OwnerMemory {
    reservation: VirtRange,
}

impl OwnerMemory {
    fn new() -> Self {
        let space = device_address_space();
        let reservation = space
            .reserve(MEMORY_BYTES as usize)
            .expect("the host can reserve an owner's linear memory");
        space
            .commit(reservation, PageFlags::READ | PageFlags::WRITE)
            .expect("the host can commit an owner's linear memory");
        Self { reservation }
    }

    /// The memory as a call that takes a span sees it.
    fn guest(&self) -> GuestMemory {
        GuestMemory::new(self.reservation.start, MEMORY_BYTES)
    }

    /// The address a linear-memory offset names.
    fn address(&self, offset: u64) -> usize {
        self.reservation.start.raw() + offset as usize
    }

    /// What the owner's address space says is behind `offset`.
    fn translate(&self, offset: u64) -> Translation {
        device_address_space().translate(VirtAddr::new(self.address(offset)))
    }

    /// The bytes the owner sees at `offset`, through its own mapping.
    fn read(&self, offset: u64, len: usize) -> Vec<u8> {
        // SAFETY: the whole reservation is committed or shown, and the
        // range is inside it.
        unsafe { std::slice::from_raw_parts(self.address(offset) as *const u8, len) }.to_vec()
    }

    /// Write `bytes` at `offset` through the owner's own mapping.
    fn write(&self, offset: u64, bytes: &[u8]) {
        // SAFETY: as `read`, and every mapping here is writable.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.address(offset) as *mut u8,
                bytes.len(),
            );
        }
    }
}

impl Drop for OwnerMemory {
    fn drop(&mut self) {
        device_address_space()
            .release(self.reservation)
            .expect("an owner's linear memory is released with it");
    }
}

fn registry() -> DeviceGrantRegistry {
    let registry = DeviceGrantRegistry::new();
    registry
        .publish(hosted_device_grants().expect("the hosted device is describable"))
        .expect("discovery publishes once");
    registry
}

/// The cap an owner the kernel granted the hosted device to holds.
fn device_cap() -> DeviceGrantClaimCap {
    let mut authority = ProcessAuthority::empty();
    authority
        .grant_device(DeviceName::new(HOSTED_DEVICE_NAME).expect("the hosted name fits"))
        .expect("one named device fits");
    authority
        .derive_device_grant_cap(HOSTED_DEVICE_NAME)
        .expect("the authority names the hosted device")
}

fn claim(registry: &DeviceGrantRegistry, views: &mut ViewTable) -> GrantLease {
    registry
        .claim(device_cap(), views.group())
        .expect("the device is free")
}

/// An instance the kernel did not grant the device to has no way to ask
/// for it, and what it is told about the real device is what it is told
/// about one the machine never published.
#[test]
fn a_claim_without_the_device_cap_is_answered_as_missing() {
    let registry = registry();
    assert!(
        registry.device(HOSTED_DEVICE_NAME).is_some(),
        "the machine publishes the device"
    );

    let ordinary = ProcessAuthority::empty();
    let published = ordinary
        .derive_device_grant_cap(HOSTED_DEVICE_NAME)
        .expect_err("an ordinary instance holds no cap for a published device");
    let unpublished = ordinary
        .derive_device_grant_cap("hosted:not-a-device")
        .expect_err("an ordinary instance holds no cap for any device");

    assert!(matches!(
        (published, unpublished),
        (
            ProcessAuthorityError::DeviceGrantExceedsAuthority(_),
            ProcessAuthorityError::DeviceGrantExceedsAuthority(_)
        )
    ));
}

fn reclaim(lease: GrantLease, views: &mut ViewTable) {
    lease
        .end_views(views)
        .expect("the pool can put memory back behind every view");
    drop(lease);
}

/// A register write through the owner's view has to reach the device.
/// That is the whole point of showing the region rather than offering a
/// call per register: the two views are the same memory.
#[test]
fn a_shown_region_is_the_device_itself_and_not_a_copy() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);

    lease
        .map_region(0, &mut views, memory.guest(), REGISTERS_AT)
        .expect("the register file is shown");

    memory.write(REGISTERS_AT.offset, b"helios");
    assert_eq!(&device_registers()[..6], b"helios");
    // And the other way: what the device writes, the owner reads.
    assert_eq!(memory.read(REGISTERS_AT.offset, 6), b"helios");
    assert_eq!(
        lease
            .map_region(0, &mut views, memory.guest(), RING_AT)
            .err(),
        Some(GrantError::RegionAlreadyMapped),
        "a second span would be a second path to the same registers"
    );
    reclaim(lease, &mut views);
}

/// An owner that lets go loses its path to the device before anyone
/// else is offered it. The span stays the owner's memory, so what has to
/// be gone is the device behind it: the owner reads zeroes there, and
/// what it writes no longer reaches the registers.
#[test]
fn reclaim_puts_zeroed_memory_back_behind_the_owner_s_view() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);
    lease
        .map_region(0, &mut views, memory.guest(), REGISTERS_AT)
        .expect("the register file is shown");
    memory.write(REGISTERS_AT.offset, b"before");

    reclaim(lease, &mut views);

    assert!(views.is_empty());
    assert!(
        matches!(
            memory.translate(REGISTERS_AT.offset),
            Translation::Committed { .. }
        ),
        "the span is still the owner's memory"
    );
    let unit = host_mapping_granule() as usize;
    assert!(
        memory
            .read(REGISTERS_AT.offset, 2 * unit)
            .iter()
            .all(|byte| *byte == 0),
        "zeroed memory, not the registers"
    );
    memory.write(REGISTERS_AT.offset, b"after!");
    assert_eq!(
        &device_registers()[..6],
        b"before",
        "the owner's writes no longer reach the device"
    );
}

/// A store torn down without letting go — an OOM kill — drops its view
/// table, and that alone takes every view of the device out of its
/// memory, leaving the spans uncommitted.
#[test]
fn dropping_the_view_table_withdraws_every_view_of_the_device() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);
    lease
        .map_region(0, &mut views, memory.guest(), REGISTERS_AT)
        .expect("the register file is shown");
    lease
        .dma_alloc(4096, &mut views, memory.guest(), RING_AT)
        .expect("the ring fits the budget");

    drop(views);

    assert_eq!(memory.translate(REGISTERS_AT.offset), Translation::Reserved);
    assert_eq!(memory.translate(RING_AT.offset), Translation::Reserved);
    drop(lease);
}

/// The device goes to one owner. A second is refused rather than
/// queued, and gets it once the first lets go.
#[test]
fn the_device_goes_to_one_owner_at_a_time() {
    let registry = registry();
    let mut first_views = ViewTable::new();
    let mut second_views = ViewTable::new();
    let first = claim(&registry, &mut first_views);

    assert_eq!(
        registry.claim(device_cap(), second_views.group()).err(),
        Some(GrantError::AlreadyClaimed)
    );

    reclaim(first, &mut first_views);
    let second = claim(&registry, &mut second_views);
    reclaim(second, &mut second_views);
}

/// A pinned ring is the owner's memory and the device's at once: the
/// owner fills it through its linear memory, and the address the kernel
/// reports is what the device issues to read the same bytes.
#[test]
fn a_pinned_ring_is_addressable_from_both_ends() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);

    let ring = lease
        .dma_alloc(4096, &mut views, memory.guest(), RING_AT)
        .expect("the ring fits the budget");

    memory.write(RING_AT.offset, b"descriptor");
    // SAFETY: the ring is pinned for as long as the lease holds it, and
    // a hosted device issues the host's own addresses.
    let through_the_device =
        unsafe { std::slice::from_raw_parts(ring.device_address as *const u8, 10) };
    assert_eq!(through_the_device, b"descriptor");
    assert_eq!(lease.stats().pinned_bytes, ring.bytes);

    reclaim(lease, &mut views);

    assert!(
        memory
            .read(RING_AT.offset, ring.bytes as usize)
            .iter()
            .all(|byte| *byte == 0),
        "the owner's span is ordinary zeroed memory again"
    );
}

/// The device holds a ring until the grant ends, even once the owner's
/// view of it is gone: a device mid-transfer still reads and writes it.
/// The run goes back to the pool when the last of the two lets go.
#[test]
fn a_ring_is_freed_only_after_the_device_lets_go() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);
    let ring = lease
        .dma_alloc(4096, &mut views, memory.guest(), RING_AT)
        .expect("the ring fits the budget");
    memory.write(RING_AT.offset, b"in flight");

    drop(views);

    // SAFETY: the lease still holds the run.
    let through_the_device =
        unsafe { std::slice::from_raw_parts(ring.device_address as *const u8, 9) };
    assert_eq!(
        through_the_device, b"in flight",
        "the device's run lives on"
    );

    drop(lease);

    // The direct translation makes the device address the run's own, so
    // freeing it again is how the test asks whether it is still held.
    assert_eq!(
        device_address_space().free_run(PhysicalRange::new(ring.device_address, ring.bytes), 4096),
        Err(AddressSpaceError::NotCommitted),
        "the run went back to the pool with the grant"
    );
}

/// A span the owner names wrongly is refused before anything is pinned,
/// so a driver that names the wrong pages costs the machine nothing.
#[test]
fn a_bad_span_is_refused_before_a_ring_is_pinned() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);

    assert_eq!(
        lease
            .dma_alloc(
                4096,
                &mut views,
                memory.guest(),
                Span::new(4096, WASM_PAGE_BYTES)
            )
            .err(),
        Some(GrantError::View(ViewError::Misaligned))
    );
    assert_eq!(
        lease
            .dma_alloc(
                4096,
                &mut views,
                memory.guest(),
                Span::new(MEMORY_BYTES, WASM_PAGE_BYTES)
            )
            .err(),
        Some(GrantError::View(ViewError::OutOfBounds {
            memory_bytes: MEMORY_BYTES
        }))
    );
    assert_eq!(lease.stats().dma_buffers, 0);
    assert_eq!(lease.stats().pinned_bytes, 0);
    assert!(views.is_empty());
    reclaim(lease, &mut views);
}

/// A driver that asks for more than the machine will pin for its device
/// is refused, rather than being allowed to squeeze every other
/// instance out of the pool.
#[test]
fn a_driver_cannot_pin_more_than_the_kernel_budgeted() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut views = ViewTable::new();
    let mut lease = claim(&registry, &mut views);
    let budget = lease.grant().dma().byte_budget;

    lease
        .dma_alloc(4096, &mut views, memory.guest(), Span::new(0, budget))
        .expect("the whole budget is available at once");

    assert_eq!(
        lease
            .dma_alloc(
                4096,
                &mut views,
                memory.guest(),
                Span::new(budget, WASM_PAGE_BYTES)
            )
            .err(),
        Some(GrantError::BudgetExhausted)
    );
    reclaim(lease, &mut views);
}

/// An interrupt is taken wherever the controller delivered it and
/// consumed wherever the owner is running. The hand-off has to survive
/// the owner parking between its inspection of the queue and its wait,
/// which is what the relay arming first buys.
#[test]
fn an_interrupt_reaches_an_owner_that_is_already_parked() {
    let registry = registry();
    let mut views = ViewTable::new();
    let lease = claim(&registry, &mut views);
    let (masked_before, unmasked_before) = interrupt_controller_counts();

    lease.relay().unmask(0).expect("the source exists");

    futures_lite::future::block_on(async {
        let owner = async { lease.relay().next_event().await };
        let controller = async {
            // Yields first, so the owner has inspected the empty queue
            // and armed its wake-up before the interrupt is raised.
            helios_kernel::yield_now().await;
            assert!(registry.forward(GrantInterrupt::new(HOSTED_DEVICE_INTERRUPT)));
        };
        let (event, ()) = futures_lite::future::zip(owner, controller).await;
        assert_eq!(event.index, 0);
        assert_eq!(event.sequence, 1);
    });

    let (masked_after, unmasked_after) = interrupt_controller_counts();
    assert_eq!(
        unmasked_after - unmasked_before,
        1,
        "the driver armed the source once"
    );
    assert_eq!(
        masked_after - masked_before,
        1,
        "the delivery held it off again, so the device cannot spin the machine"
    );
    assert_eq!(lease.stats().interrupts.forwarded, 1);
    reclaim(lease, &mut views);
}

/// A device region shown inside committed memory is the device itself,
/// below the span's own bounds, and ending the view puts zeroed memory
/// back without touching the device.
#[test]
fn a_substituted_device_region_is_the_device_and_restore_leaves_it_alone() {
    use helios_hal::device::{DeviceRegion, DeviceRegionAttributes};

    let space = device_address_space();
    let unit = host_mapping_granule() as usize;
    let range = space.reserve(2 * unit).expect("the host can reserve");
    space
        .commit(range, PageFlags::READ | PageFlags::WRITE)
        .expect("the host can commit");
    let registers = device_registers();
    let region = DeviceRegion::new(
        PhysicalRange::new(registers.as_ptr() as u64, unit as u64),
        DeviceRegionAttributes::REGISTERS,
    );
    let view = VirtRange::new(range.start, unit);

    space
        .substitute_device(view, region)
        .expect("a hosted device's region can be shown");

    let shown = view.start.raw() as *mut u8;
    // SAFETY: the view is mapped read-write and `unit` bytes long.
    unsafe { std::ptr::copy_nonoverlapping(b"subst!".as_ptr(), shown, 6) };
    assert_eq!(&device_registers()[..6], b"subst!", "owner to device");
    assert_eq!(
        space.substitute_device(view, region),
        Err(AddressSpaceError::Substituted)
    );

    space.restore(view).expect("the pool can refill the view");

    // SAFETY: the restored view is mapped read-write and `unit` bytes
    // long.
    let restored = unsafe { std::slice::from_raw_parts(shown, unit) };
    assert!(restored.iter().all(|byte| *byte == 0), "zeroed memory");
    assert_eq!(
        &device_registers()[..6],
        b"subst!",
        "the device is untouched"
    );
    space
        .release(range)
        .expect("the owner's memory is released");
}
