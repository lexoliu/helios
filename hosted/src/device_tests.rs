//! What a granted device does, driven through the hosted machine.
//!
//! These exercise the kernel's device path against a real address space
//! rather than a recording one: the mappings are host mappings, the
//! pinned buffers are host memory, and the alias a granted region
//! creates is provable by writing through one view and reading the
//! other. What the kernel's own tests assert about bookkeeping, these
//! assert about the memory underneath it.

use helios_hal::vmm::{AddressSpace, Translation, VirtAddr, VirtRange};
use helios_kernel::{
    DEVICE_WINDOW_BYTES, DeviceGrantRegistry, DeviceWindow, GrantError, GrantInterrupt, GrantLease,
};

use crate::device::{
    HOSTED_DEVICE_INTERRUPT, HOSTED_DEVICE_NAME, device_address_space, device_registers,
    hosted_device_grants, interrupt_controller_counts,
};

/// A linear-memory reservation large enough to carry a device window
/// and leave the owner room below it.
const RESERVATION_BYTES: usize = (DEVICE_WINDOW_BYTES as usize) * 4;

/// One owner's linear memory, released when the test drops it.
struct OwnerMemory {
    reservation: VirtRange,
}

impl OwnerMemory {
    fn new() -> Self {
        let reservation = device_address_space()
            .reserve(RESERVATION_BYTES)
            .expect("the host can reserve an owner's linear memory");
        Self { reservation }
    }

    fn window(&self) -> DeviceWindow {
        DeviceWindow::top_of(self.reservation.start, RESERVATION_BYTES as u64)
    }

    /// The address a linear-memory offset names.
    fn address(&self, offset: u64) -> usize {
        self.reservation.start.raw() + offset as usize
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

/// The bytes an owner sees at `offset`, through its own mapping.
fn owner_read(memory: &OwnerMemory, offset: u64, len: usize) -> Vec<u8> {
    // SAFETY: the caller has mapped `offset` and the range is inside the
    // owner's reservation.
    unsafe { std::slice::from_raw_parts(memory.address(offset) as *const u8, len) }.to_vec()
}

/// Write `bytes` at `offset` through the owner's own mapping.
fn owner_write(memory: &OwnerMemory, offset: u64, bytes: &[u8]) {
    // SAFETY: as `owner_read`, and the mapping is writable.
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            memory.address(offset) as *mut u8,
            bytes.len(),
        );
    }
}

/// A register write through the owner's mapping has to reach the
/// device. That is the whole point of mapping the region rather than
/// offering a call per register: the two views are the same memory.
#[test]
fn a_mapped_region_is_the_device_itself_and_not_a_copy() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut lease = registry
        .claim(HOSTED_DEVICE_NAME, memory.window())
        .expect("the device is free");

    let placement = lease.map_region(0).expect("the register file maps");

    assert_eq!(placement.offset, memory.window().offset());
    owner_write(&memory, placement.offset, b"helios");
    assert_eq!(&device_registers()[..6], b"helios");
    // And the other way: what the device writes, the owner reads.
    assert_eq!(owner_read(&memory, placement.offset, 6), b"helios");
}

/// An owner that dies loses its path to the device before anyone else
/// is offered it. The reservation is still the owner's address space,
/// so what has to be gone is the mapping inside it.
#[test]
fn reclaim_takes_the_owner_s_path_to_the_device_away() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut lease = registry
        .claim(HOSTED_DEVICE_NAME, memory.window())
        .expect("the device is free");
    let placement = lease.map_region(0).expect("the register file maps");
    let mapped = VirtAddr::new(memory.address(placement.offset));

    assert!(matches!(
        device_address_space().translate(mapped),
        Translation::Committed { .. }
    ));

    lease.reclaim();

    assert!(
        matches!(
            device_address_space().translate(mapped),
            Translation::Reserved
        ),
        "the address stays the owner's, but nothing is behind it any more"
    );
}

/// The device goes to one owner. A second is refused rather than
/// queued, and gets it once the first lets go.
#[test]
fn the_device_goes_to_one_owner_at_a_time() {
    let registry = registry();
    let first_memory = OwnerMemory::new();
    let second_memory = OwnerMemory::new();
    let first = registry
        .claim(HOSTED_DEVICE_NAME, first_memory.window())
        .expect("the first owner gets the device");

    assert_eq!(
        registry
            .claim(HOSTED_DEVICE_NAME, second_memory.window())
            .err(),
        Some(GrantError::AlreadyClaimed)
    );

    drop(first);
    let second = registry
        .claim(HOSTED_DEVICE_NAME, second_memory.window())
        .expect("the device comes back when its owner lets go");
    drop(second);
}

/// A pinned buffer is the owner's memory and the device's at once: the
/// owner fills it through its linear memory, and the address the kernel
/// reports is what the device would issue to read the same bytes.
#[test]
fn a_pinned_buffer_is_addressable_from_both_ends_and_released_on_death() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut lease = registry
        .claim(HOSTED_DEVICE_NAME, memory.window())
        .expect("the device is free");

    let buffer = lease
        .dma_alloc(4096, 4096)
        .expect("the ring fits the budget");

    owner_write(&memory, buffer.offset, b"descriptor");
    assert_eq!(
        buffer.device_address,
        memory.address(buffer.offset) as u64,
        "a hosted device issues the host's own addresses"
    );
    // SAFETY: the buffer is committed and covers the read.
    let through_the_device =
        unsafe { std::slice::from_raw_parts(buffer.device_address as *const u8, 10) };
    assert_eq!(through_the_device, b"descriptor");
    assert_eq!(lease.stats().pinned_bytes, buffer.bytes);

    let pinned = VirtAddr::new(memory.address(buffer.offset));
    lease.reclaim();

    assert!(
        matches!(
            device_address_space().translate(pinned),
            Translation::Reserved
        ),
        "a dead owner's pins go back to the pool"
    );
}

/// A driver that asks for more than the machine will pin for its device
/// is refused, rather than being allowed to squeeze every other
/// instance out of the pool.
#[test]
fn a_driver_cannot_pin_more_than_the_kernel_budgeted() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let mut lease = registry
        .claim(HOSTED_DEVICE_NAME, memory.window())
        .expect("the device is free");
    let budget = lease.grant().dma().byte_budget;

    lease
        .dma_alloc(budget, 4096)
        .expect("the whole budget is available at once");

    assert_eq!(
        lease.dma_alloc(4096, 4096).err(),
        Some(GrantError::BudgetExhausted)
    );
    lease.reclaim();
}

/// An interrupt is taken wherever the controller delivered it and
/// consumed wherever the owner is running. The hand-off has to survive
/// the owner parking between its inspection of the queue and its wait,
/// which is what the relay arming first buys.
#[test]
fn an_interrupt_reaches_an_owner_that_is_already_parked() {
    let registry = registry();
    let memory = OwnerMemory::new();
    let lease: GrantLease = registry
        .claim(HOSTED_DEVICE_NAME, memory.window())
        .expect("the device is free");
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
    lease.reclaim();
}
