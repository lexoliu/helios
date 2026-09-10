//! The surface registry's own tests.
//!
//! What is checked here is the part of the path that has no compositor
//! in it: minting and retiring a window, the geometry arithmetic a
//! `commit` is validated against, and the queue that carries the input
//! the compositor routed. The arena is checked against the recording
//! platform surface in [`crate::device::test_hooks`], so a test asserts
//! the mappings the kernel actually asked the address space for.

use core::task::{Context, Poll, Waker};

use helios_hal::input::InputEvent;
use helios_hal::iommu::PhysicalRange;
use helios_hal::pmm::PhysFrame;
use helios_hal::vmm::VirtAddr;

use crate::device::test_hooks::MappingChange;
use crate::device::{DeviceWindow, test_hooks};
use crate::pins::PinnedArena;

use super::service::{SurfaceGeometry, SurfaceRect, SurfaceService, SurfaceShared};

/// Reservations the kernel builds are four gigabytes; the tests use the
/// same shape so the windows land where they do on a real instance.
const RESERVATION_BYTES: u64 = 1 << 32;
const WINDOW_BASE: usize = 0x1_0000_0000;

fn window() -> DeviceWindow {
    DeviceWindow::top_of(VirtAddr::new(WINDOW_BASE), RESERVATION_BYTES)
        .below(crate::device::DISPLAY_WINDOW_BYTES)
        .below(crate::device::SURFACE_WINDOW_BYTES)
}

fn geometry(width: u32, height: u32) -> SurfaceGeometry {
    SurfaceGeometry { width, height }
}

fn key(code: u16, value: i32) -> InputEvent {
    InputEvent {
        kind: helios_hal::input::codes::EV_KEY,
        code,
        value,
    }
}

#[test]
fn a_frame_is_four_bytes_a_pixel_and_a_degenerate_one_has_no_size() {
    assert_eq!(geometry(4, 3).frame_bytes(), Some(48));
    assert_eq!(geometry(0, 3).frame_bytes(), None);
    assert_eq!(geometry(4, 0).frame_bytes(), None);
    assert_eq!(geometry(u32::MAX, u32::MAX).frame_bytes(), None);
}

#[test]
fn a_region_fits_exactly_when_it_lies_inside_the_surface() {
    let surface = geometry(64, 32);
    let rect = |x, y, width, height| SurfaceRect {
        x,
        y,
        width,
        height,
    };
    assert!(rect(0, 0, 64, 32).fits(surface));
    assert!(rect(63, 31, 1, 1).fits(surface));
    // One pixel past the right edge, one past the bottom, and an empty
    // rectangle: all three are a caller's bug rather than a no-op, so
    // none of them fits.
    assert!(!rect(1, 0, 64, 32).fits(surface));
    assert!(!rect(0, 1, 64, 32).fits(surface));
    assert!(!rect(0, 0, 0, 0).fits(surface));
    // An origin that overflows when the width is added to it names no
    // region at all.
    assert!(!rect(u32::MAX, 0, 1, 1).fits(surface));
}

#[test]
fn a_registered_surface_is_live_until_it_is_unregistered() {
    let service = SurfaceService::new();
    let surface = service.register(geometry(32, 16)).expect("room for one");
    assert_eq!(service.live(), 1);
    assert_eq!(service.created(), 1);
    assert!(surface.is_alive());
    assert_eq!(surface.geometry(), geometry(32, 16));
    assert!(service.lookup(surface.id()).is_some());

    service.unregister(surface.id());
    assert_eq!(service.live(), 0);
    assert!(!surface.is_alive());
    assert!(service.lookup(surface.id()).is_none());
}

#[test]
fn a_dead_compositor_takes_every_window_off_the_desktop() {
    let service = SurfaceService::new();
    let first = service.register(geometry(8, 8)).expect("room");
    let second = service.register(geometry(8, 8)).expect("room");

    service.retire_compositor();

    assert!(!first.is_alive());
    assert!(!second.is_alive());
    assert_eq!(service.live(), 0);
}

#[test]
fn only_the_instance_the_supervisor_named_is_the_compositor() {
    let service = SurfaceService::new();
    let registry = crate::InstanceRegistry::new();
    let compositor = registry.register("compositor", 0);
    let other = registry.register("client", 0);

    // Before a compositor exists nobody is it, so `deliver` refuses
    // everybody rather than letting the first caller win.
    assert!(!service.is_compositor(compositor.id()));

    service.set_compositor(compositor.id());
    assert!(service.is_compositor(compositor.id()));
    assert!(!service.is_compositor(other.id()));

    service.retire_compositor();
    assert!(!service.is_compositor(compositor.id()));
}

#[test]
fn a_routed_report_reaches_the_surfaces_reader_whole() {
    let service = SurfaceService::new();
    let surface = service.register(geometry(8, 8)).expect("room");
    let mut events = SurfaceShared::events(&surface);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(events.poll_burst(&mut context), Poll::Pending));

    let report = [key(30, 1), key(30, 0)];
    assert!(surface.publish_report(&report));

    let burst = match events.poll_burst(&mut context) {
        Poll::Ready(Some(burst)) => burst,
        Poll::Ready(None) => panic!("a live surface's reader does not end"),
        Poll::Pending => panic!("a published report is readable rather than pending"),
    };
    assert_eq!(burst.as_slice(), report.as_slice());
}

#[test]
fn a_report_no_reader_could_hold_is_dropped_whole_rather_than_split() {
    let service = SurfaceService::new();
    let surface = service.register(geometry(8, 8)).expect("room");

    // Fill the queue to one event short of full, then offer a report of
    // two: half of it would fit, and half of a report is what this path
    // exists to prevent.
    let filler = [key(30, 1)];
    for _ in 0..super::SURFACE_EVENT_QUEUE_DEPTH - 1 {
        assert!(surface.publish_report(&filler));
    }
    assert!(!surface.publish_report(&[key(31, 1), key(31, 0)]));

    let mut events = SurfaceShared::events(&surface);
    let mut context = Context::from_waker(Waker::noop());
    let burst = match events.poll_burst(&mut context) {
        Poll::Ready(Some(burst)) => burst,
        _ => panic!("the queue holds what was accepted"),
    };
    assert_eq!(burst.len(), super::SURFACE_EVENT_QUEUE_DEPTH - 1);
    assert!(burst.iter().all(|event| event.code == 30));
}

#[test]
fn a_retired_surfaces_reader_ends_rather_than_parking_forever() {
    let service = SurfaceService::new();
    let surface = service.register(geometry(8, 8)).expect("room");
    let mut events = SurfaceShared::events(&surface);
    let mut context = Context::from_waker(Waker::noop());

    service.unregister(surface.id());

    assert!(matches!(events.poll_burst(&mut context), Poll::Ready(None)));
}

#[test]
fn a_surfaces_pages_are_committed_once_and_viewed_once() {
    test_hooks::install();
    let before = test_hooks::changes().len();

    let mut client = PinnedArena::<4>::new(window());
    let frame = client
        .pin(geometry(64, 64).frame_bytes().expect("a real geometry"))
        .expect("the window has room");
    assert!(!frame.is_shared());

    let mut compositor = PinnedArena::<4>::new(window());
    let view = compositor
        .map(frame.physical())
        .expect("the address space can hand out a second view");
    assert!(view.is_shared());
    // The same physical run, at each instance's own offset: this is the
    // whole of what "the compositor composes from the client's bytes"
    // means.
    assert_eq!(view.physical(), frame.physical());
    assert_eq!(view.bytes, frame.bytes);

    let changes = test_hooks::changes();
    assert!(matches!(changes[before], MappingChange::Commit(_)));
    assert!(matches!(
        changes[before + 1],
        MappingChange::MapShared(_, physical) if physical == frame.physical()
    ));

    // The view goes first and frees nothing; the run itself goes back to
    // the client's pool.
    drop(compositor);
    drop(client);
    let changes = test_hooks::changes();
    assert!(matches!(changes[before + 2], MappingChange::UnmapShared(_)));
    assert!(matches!(changes[before + 3], MappingChange::Released(_)));
}

#[test]
fn a_frames_physical_range_covers_exactly_the_pages_it_was_given() {
    test_hooks::install();
    let mut client = PinnedArena::<4>::new(window());
    let frame = client.pin(PhysFrame::SIZE as u64 + 1).expect("room");
    // Rounded up to whole granules, and the physical range says the same
    // thing the mapping does.
    assert_eq!(frame.bytes, 2 * PhysFrame::SIZE as u64);
    assert_eq!(
        frame.physical(),
        PhysicalRange::new(frame.backing.start.phys_addr() as u64, frame.bytes)
    );
}
