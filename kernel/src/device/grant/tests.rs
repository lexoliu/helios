//! The reclaim path, exercised against a platform double.
//!
//! These run on the host in `just test-units` because the property
//! being checked is the state machine's, not the hardware's: that a
//! driver dying at any point leaves the kernel holding nothing. The
//! same code driving real registers is what the aarch64 boot proves.

use alloc::{vec, vec::Vec};
use core::cell::RefCell;

use helios_hal::{
    device::{DeviceRegion, DmaCapable, RegionAttribute},
    iommu::DomainId,
    pmm::{PhysFrame, PhysFrameRange},
    vmm::{PageFlags, VirtAddr, VirtRange},
};
use thiserror::Error;

use super::{DeviceGrant, DeviceGrantError, DeviceHost, GuestWindow};

/// Every platform operation a grant performs, in the order it happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Quiesce,
    Reserve { offset: u64, byte_len: usize },
    Release { offset: u64 },
    Map { offset: u64, device: bool },
    Unmap { offset: u64 },
    Mask(u32),
    Unmask(u32),
    Pin { frame_count: usize },
    Unpin { start: usize },
    Translate,
    Withdraw,
    Detach(DomainId),
}

#[derive(Debug, Error)]
#[error("the platform double was told to fail")]
struct FakeError;

struct FakeHost {
    steps: RefCell<Vec<Step>>,
    /// Next free byte offset in the pretend linear memory. Starts past
    /// zero so an offset of zero in an assertion is never accidental.
    next_offset: RefCell<u64>,
    /// Next free frame, from a pool the double never reuses, so a
    /// double-free shows up as a mismatched index.
    next_frame: RefCell<usize>,
    /// Frames currently pinned, so a test can assert the pool ends empty.
    pinned: RefCell<Vec<usize>>,
    quiesce_fails: bool,
    detach_fails: bool,
}

impl FakeHost {
    fn new() -> Self {
        Self {
            steps: RefCell::new(Vec::new()),
            next_offset: RefCell::new(0x1_0000),
            next_frame: RefCell::new(0x800),
            pinned: RefCell::new(Vec::new()),
            quiesce_fails: false,
            detach_fails: false,
        }
    }

    fn record(&self, step: Step) {
        self.steps.borrow_mut().push(step);
    }
}

impl DeviceHost for &FakeHost {
    type Source = u32;
    type Error = FakeError;

    fn quiesce(&self) -> Result<(), FakeError> {
        self.record(Step::Quiesce);
        if self.quiesce_fails {
            return Err(FakeError);
        }
        Ok(())
    }

    fn reserve_window(&self, byte_len: usize) -> Result<GuestWindow, FakeError> {
        let mut next = self.next_offset.borrow_mut();
        let offset = *next;
        *next += byte_len as u64;
        self.record(Step::Reserve { offset, byte_len });
        Ok(GuestWindow {
            offset,
            virt: VirtRange::new(VirtAddr::new(0x4000_0000 + offset as usize), byte_len),
        })
    }

    fn release_window(&self, window: GuestWindow) {
        self.record(Step::Release {
            offset: window.offset,
        });
    }

    fn map_physical(
        &self,
        window: GuestWindow,
        _physical: PhysFrameRange,
        flags: PageFlags,
    ) -> Result<(), FakeError> {
        self.record(Step::Map {
            offset: window.offset,
            device: flags.contains(PageFlags::DEVICE),
        });
        Ok(())
    }

    fn unmap_physical(&self, window: GuestWindow) -> Result<(), FakeError> {
        self.record(Step::Unmap {
            offset: window.offset,
        });
        Ok(())
    }

    fn mask(&self, source: u32) {
        self.record(Step::Mask(source));
    }

    fn unmask(&self, source: u32) {
        self.record(Step::Unmask(source));
    }

    fn pin_frames(&self, frame_count: usize) -> Result<PhysFrameRange, FakeError> {
        let mut next = self.next_frame.borrow_mut();
        let start = *next;
        *next += frame_count;
        self.record(Step::Pin { frame_count });
        self.pinned.borrow_mut().push(start);
        Ok(PhysFrameRange {
            start: PhysFrame::from_index(start),
            frame_count,
        })
    }

    fn unpin_frames(&self, frames: PhysFrameRange) {
        let start = frames.start.index();
        self.record(Step::Unpin { start });
        let mut pinned = self.pinned.borrow_mut();
        let position = pinned
            .iter()
            .position(|candidate| *candidate == start)
            .expect("frames were unpinned twice, or were never pinned");
        pinned.remove(position);
    }

    fn device_address(
        &self,
        _domain: Option<DomainId>,
        frames: PhysFrameRange,
    ) -> Result<u64, FakeError> {
        self.record(Step::Translate);
        Ok(frames.start.phys_addr() as u64)
    }

    fn withdraw_device_address(&self, _domain: Option<DomainId>, _address: u64, _byte_len: usize) {
        self.record(Step::Withdraw);
    }

    fn detach_domain(&self, domain: DomainId) -> Result<(), FakeError> {
        self.record(Step::Detach(domain));
        if self.detach_fails {
            return Err(FakeError);
        }
        Ok(())
    }
}

/// A NIC's registers as firmware describes them: not page-aligned, so
/// the mapping is page-rounded and the offset is not the window's base.
const NIC_REGISTERS: DeviceRegion = DeviceRegion::registers(0x1000_0080, 0x200);
const NIC_MEMORY: DeviceRegion = DeviceRegion {
    physical_base: 0x2000_0000,
    byte_len: 0x1000,
    attribute: RegionAttribute::Memory,
};

fn confined() -> DmaCapable {
    DmaCapable {
        address_width_bits: 64,
        domain: Some(DomainId::new(7)),
    }
}

fn grant(host: &FakeHost, dma: DmaCapable) -> DeviceGrant<&FakeHost> {
    DeviceGrant::new(
        host,
        &[NIC_REGISTERS, NIC_MEMORY],
        &[42, 43],
        dma,
        16 * PhysFrame::SIZE,
    )
    .expect("the double stays inside every grant limit")
}

#[test]
fn a_register_window_lands_at_its_page_offset_in_the_driver_s_memory() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());

    let mapped = grant.map_region(0).expect("region 0 exists");

    assert_eq!(mapped.byte_len, 0x200);
    assert_eq!(
        mapped.offset, 0x1_0080,
        "the driver is pointed at the registers, not at the page they start in"
    );
    assert_eq!(
        host.steps.borrow().as_slice(),
        [
            Step::Reserve {
                offset: 0x1_0000,
                byte_len: PhysFrame::SIZE
            },
            Step::Map {
                offset: 0x1_0000,
                device: true
            },
        ],
        "registers map with the device attribute"
    );
}

#[test]
fn a_memory_region_maps_without_the_device_attribute() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());

    grant.map_region(1).expect("region 1 exists");

    assert!(
        host.steps.borrow().contains(&Step::Map {
            offset: 0x1_0000,
            device: false
        }),
        "a prefetchable window is ordinary memory and must stay cacheable"
    );
}

#[test]
fn mapping_a_region_twice_is_refused() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());
    let first = grant.map_region(0).expect("region 0 exists");

    let again = grant.map_region(0);

    assert!(matches!(
        again,
        Err(DeviceGrantError::RegionAlreadyMapped { index: 0, offset })
            if offset == first.offset
    ));
}

#[test]
fn a_region_this_device_does_not_have_is_refused() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());

    assert!(matches!(
        grant.map_region(4),
        Err(DeviceGrantError::UnknownRegion { index: 4 })
    ));
}

#[test]
fn frames_out_of_the_engine_s_reach_are_refused_and_returned() {
    let host = FakeHost::new();
    // The double's pool starts at frame 0x800, above what a 20-bit
    // engine can address.
    let mut grant = grant(
        &host,
        DmaCapable {
            address_width_bits: 20,
            domain: None,
        },
    );

    let allocated = grant.dma_alloc(PhysFrame::SIZE);

    assert!(matches!(
        allocated,
        Err(DeviceGrantError::UnaddressableDma {
            address_width_bits: 20,
            ..
        })
    ));
    assert!(
        host.pinned.borrow().is_empty(),
        "frames the device cannot address go straight back to the allocator"
    );
}

#[test]
fn the_dma_budget_bounds_what_a_driver_can_pin() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());
    grant
        .dma_alloc(16 * PhysFrame::SIZE)
        .expect("the whole budget in one region is allowed");

    let past_it = grant.dma_alloc(1);

    assert!(matches!(
        past_it,
        Err(DeviceGrantError::DmaBudgetExhausted {
            requested,
            remaining: 0
        }) if requested == PhysFrame::SIZE
    ));
}

#[test]
fn a_freed_dma_region_gives_its_budget_back() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());
    let region = grant
        .dma_alloc(16 * PhysFrame::SIZE)
        .expect("within budget");
    assert_eq!(grant.dma_bytes_in_use(), 16 * PhysFrame::SIZE);

    grant.dma_free(region.offset).expect("the driver holds it");

    assert_eq!(grant.dma_bytes_in_use(), 0);
    assert!(host.pinned.borrow().is_empty());
    grant
        .dma_alloc(16 * PhysFrame::SIZE)
        .expect("the budget came back");
}

#[test]
fn freeing_a_dma_region_the_driver_does_not_hold_is_refused() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());

    assert!(matches!(
        grant.dma_free(0xdead_0000),
        Err(DeviceGrantError::UnknownDmaRegion {
            offset: 0xdead_0000
        })
    ));
}

#[test]
fn killing_a_driver_mid_dma_returns_every_resource() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());
    grant.map_region(0).expect("region 0 exists");
    grant.map_region(1).expect("region 1 exists");
    grant.unmask(42).expect("42 is this device's");
    let in_flight = grant.dma_alloc(4 * PhysFrame::SIZE).expect("within budget");
    host.steps.borrow_mut().clear();

    // The driver dies here, with its registers mapped, its interrupt
    // live, and the device writing into `in_flight`.
    let report = grant.reclaim();

    assert_eq!(report.regions_unmapped, 2);
    assert_eq!(report.sources_masked, 2);
    assert_eq!(report.dma_returned, 1);
    assert_eq!(report.dma_quarantined, 0);
    assert!(report.detached);
    assert_eq!(grant.dma_bytes_in_use(), 0);
    assert!(
        host.pinned.borrow().is_empty(),
        "the frames the device was writing into are back in the allocator"
    );

    let steps = host.steps.borrow();
    let ordinal = |wanted: Step| {
        steps
            .iter()
            .position(|step| *step == wanted)
            .unwrap_or_else(|| panic!("reclaim never performed {wanted:?}"))
    };
    assert!(
        ordinal(Step::Quiesce) < ordinal(Step::Mask(42)),
        "the device stops mastering the bus before anything else moves"
    );
    assert!(
        ordinal(Step::Mask(43)) < ordinal(Step::Detach(DomainId::new(7))),
        "interrupts are masked before the endpoint leaves its domain"
    );
    assert!(
        ordinal(Step::Detach(DomainId::new(7)))
            < ordinal(Step::Unmap {
                offset: in_flight.offset
            }),
        "DMA in flight is made to fault before its target is unmapped"
    );
    assert!(
        ordinal(Step::Unmap {
            offset: in_flight.offset
        }) < ordinal(Step::Unpin {
            start: in_flight.device_address as usize / PhysFrame::SIZE
        }),
        "frames leave the dead instance's memory before they go back to the pool"
    );
}

#[test]
fn a_device_that_cannot_be_stopped_keeps_its_frames_quarantined() {
    let host = FakeHost {
        quiesce_fails: true,
        ..FakeHost::new()
    };
    // No IOMMU, so detaching cannot prove the device stopped either.
    let mut grant = grant(&host, DmaCapable::UNCONFINED_64);
    let in_flight = grant.dma_alloc(2 * PhysFrame::SIZE).expect("within budget");

    let report = grant.reclaim();

    assert_eq!(report.dma_returned, 0);
    assert_eq!(
        report.dma_quarantined, 1,
        "handing these frames to the next allocation is a write-after-free \
         with a bus master on the other end"
    );
    assert!(!report.detached);
    assert_eq!(
        grant.quarantined().collect::<Vec<_>>(),
        vec![PhysFrameRange {
            start: PhysFrame::from_phys_addr(in_flight.device_address as usize),
            frame_count: 2
        }]
    );
    assert_eq!(
        host.pinned.borrow().len(),
        1,
        "the allocator still counts them as taken"
    );

    // Once the owner has power-cycled the device they are ordinary free
    // frames again.
    assert_eq!(grant.release_quarantine(), 1);
    assert!(host.pinned.borrow().is_empty());
}

#[test]
fn an_endpoint_that_will_not_detach_still_gives_up_everything_else() {
    let host = FakeHost {
        detach_fails: true,
        ..FakeHost::new()
    };
    let mut grant = grant(&host, confined());
    grant.map_region(0).expect("region 0 exists");
    grant.dma_alloc(PhysFrame::SIZE).expect("within budget");

    let report = grant.reclaim();

    assert!(!report.detached);
    assert_eq!(report.regions_unmapped, 1);
    assert_eq!(
        report.dma_returned, 1,
        "quiesce succeeded, so the device is stopped even though the domain refused"
    );
    assert!(host.pinned.borrow().is_empty());
}

#[test]
#[should_panic(expected = "a reclaimed device grant was used again")]
fn a_reclaimed_grant_refuses_to_be_used() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());
    grant.reclaim();

    let _ = grant.map_region(0);
}

#[test]
fn an_interrupt_this_device_does_not_raise_is_refused() {
    let host = FakeHost::new();
    let mut grant = grant(&host, confined());

    assert!(matches!(
        grant.unmask(99),
        Err(DeviceGrantError::UnknownInterrupt)
    ));
}
