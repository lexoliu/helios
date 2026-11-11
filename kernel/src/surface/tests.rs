//! The surface registry's own tests.
//!
//! What is checked here is the part of the path that has no compositor
//! in it: minting and retiring a window, the geometry arithmetic a
//! `commit` is validated against, and the queue that carries the input
//! the compositor routed. The views are checked against the recording
//! platform surface in [`crate::device::test_hooks`], so a test asserts
//! the substitutions the kernel actually asked the address space for.

use core::task::{Context, Poll, Waker};

use helios_hal::input::InputEvent;
use helios_hal::vmm::VirtAddr;

use crate::buffer::{GuestMemory, Span, ViewError, ViewTable, WASM_PAGE_BYTES};
use crate::device::test_hooks::{self, MappingChange};

use super::SurfaceServiceError;
use super::instance::SurfaceOwnership;
use super::service::{
    SURFACE_REQUEST_QUEUE_DEPTH, SurfaceGeometry, SurfaceRect, SurfaceService, SurfaceShared,
};

/// Where the tests pretend the client's and the compositor's memories
/// sit, and how far each has grown.
const CLIENT_BASE: usize = 0x1_0000_0000;
const COMPOSITOR_BASE: usize = 0x2_0000_0000;
const MEMORY_BYTES: u64 = 64 * WASM_PAGE_BYTES;

fn client_memory() -> GuestMemory {
    GuestMemory::new(VirtAddr::new(CLIENT_BASE), MEMORY_BYTES)
}

fn compositor_memory() -> GuestMemory {
    GuestMemory::new(VirtAddr::new(COMPOSITOR_BASE), MEMORY_BYTES)
}

/// `pages` wasm pages starting at page `first`.
const fn pages(first: u64, pages: u64) -> Span {
    Span::new(first * WASM_PAGE_BYTES, pages * WASM_PAGE_BYTES)
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

/// One run, pinned once and shown twice: at the client's span and at the
/// compositor's. It goes back to the pool only when both views have
/// ended, whichever ends last.
#[test]
fn a_window_is_one_buffer_shown_in_the_client_and_in_the_compositor() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, mut client_views) = (SurfaceOwnership::new(), ViewTable::new());
    let (mut compositor, mut compositor_views) = (SurfaceOwnership::new(), ViewTable::new());
    let (before, runs) = (test_hooks::changes().len(), test_hooks::live_runs());

    let pending = client
        .prepare(
            &service,
            &client_views,
            client_memory(),
            pages(2, 1),
            geometry(64, 64),
        )
        .expect("a 64 by 64 window fits one wasm page");
    let id = pending.id();
    // The order the supervisor and the client's last step run in: the
    // compositor's view is taken while the client's create is waiting
    // for the answer, and the client's own view after it.
    let buffer = pending.buffer().clone();
    compositor
        .show_view(
            &mut compositor_views,
            compositor_memory(),
            pages(5, 1),
            id,
            &buffer,
        )
        .expect("the compositor's span is free");
    let (shared, view) = client
        .show(&mut client_views, client_memory(), pending)
        .expect("nothing else was shown at the client's span");
    let physical = buffer.physical();
    drop(buffer);

    // The same physical run behind each instance's own span: this is the
    // whole of what "the compositor composes from the client's bytes"
    // means.
    let changes = test_hooks::changes();
    assert_eq!(changes[before], MappingChange::Allocated(physical));
    assert!(matches!(
        changes[before + 1],
        MappingChange::SubstituteMemory(virt, run)
            if virt.start.raw() == COMPOSITOR_BASE + 5 * WASM_PAGE_BYTES as usize
                && run == physical
    ));
    assert!(matches!(
        changes[before + 2],
        MappingChange::SubstituteMemory(virt, run)
            if virt.start.raw() == CLIENT_BASE + 2 * WASM_PAGE_BYTES as usize && run == physical
    ));
    assert!(
        service.take_returned().is_empty(),
        "a create that finished hands nothing back"
    );

    // The client lets go first; the compositor's view still holds the
    // run.
    client_views
        .end(view)
        .expect("the test pool refills anything");
    client.abandon(&service, id);
    assert!(!shared.is_alive());
    assert_eq!(test_hooks::live_runs() - runs, 1);

    compositor
        .end_view(id, &mut compositor_views)
        .expect("the test pool refills anything");
    let changes = test_hooks::changes();
    assert!(matches!(changes[before + 3], MappingChange::Restore(_)));
    assert!(matches!(changes[before + 4], MappingChange::Restore(_)));
    assert_eq!(changes[before + 5], MappingChange::Freed(physical));
    assert_eq!(test_hooks::live_runs(), runs);
}

/// A span the table would refuse costs nothing: no run is pinned and no
/// window is registered.
#[test]
fn a_bad_span_is_refused_before_anything_is_pinned_or_registered() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, views) = (SurfaceOwnership::new(), ViewTable::new());
    let runs = test_hooks::live_runs();

    let refused = client.prepare(
        &service,
        &views,
        client_memory(),
        pages(0, 2),
        geometry(64, 64),
    );
    assert_eq!(
        refused.err(),
        Some(SurfaceServiceError::BadSpan(ViewError::WrongLength {
            expected: WASM_PAGE_BYTES,
            given: 2 * WASM_PAGE_BYTES,
        }))
    );
    assert_eq!(test_hooks::live_runs(), runs);
    assert_eq!(service.live(), 0);
    assert!(views.is_empty());
}

/// A client that dies hands its windows' identities to the supervisor
/// and takes them off the desktop; its own views go with its table.
#[test]
fn a_dying_client_hands_its_windows_to_the_supervisor() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, mut views) = (SurfaceOwnership::new(), ViewTable::new());
    let pending = client
        .prepare(
            &service,
            &views,
            client_memory(),
            pages(1, 1),
            geometry(64, 64),
        )
        .expect("a 64 by 64 window fits one wasm page");
    let (shared, _view) = client
        .show(&mut views, client_memory(), pending)
        .expect("nothing else was shown at the client's span");
    let id = shared.id();
    drop(shared);

    drop(client);
    assert!(service.lookup(id).is_none());
    let returned = service.take_returned();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].ids.as_slice(), [id].as_slice());

    let before = test_hooks::changes().len();
    drop(views);
    let changes = test_hooks::changes();
    assert!(matches!(changes[before], MappingChange::Withdraw(_)));
    assert!(matches!(changes[before + 1], MappingChange::Freed(_)));
}

/// A create its client stopped waiting for — the guest dropped the
/// import's future while the compositor was answering — leaves nothing
/// shown in the client's memory, and its window reaches the supervisor
/// exactly once: not again when the client dies, and not as a slot the
/// client holds forever.
#[test]
fn a_create_its_client_stopped_waiting_for_shows_nothing_and_goes_back_once() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, views) = (SurfaceOwnership::new(), ViewTable::new());
    let (before, runs) = (test_hooks::changes().len(), test_hooks::live_runs());

    let pending = client
        .prepare(
            &service,
            &views,
            client_memory(),
            pages(1, 1),
            geometry(64, 64),
        )
        .expect("a 64 by 64 window fits one wasm page");
    let id = pending.id();
    assert_eq!(client.held(), 1);

    // The host call's future is dropped mid-await, and the pending
    // window with it.
    drop(pending);

    assert!(views.is_empty(), "nothing was shown in the client's memory");
    assert!(
        !test_hooks::changes()[before..]
            .iter()
            .any(|change| matches!(change, MappingChange::SubstituteMemory(..))),
        "no span of any memory was substituted"
    );
    assert!(service.lookup(id).is_none(), "the window left the desktop");
    let returned = service.take_returned();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].ids.as_slice(), [id].as_slice());
    assert_eq!(
        test_hooks::live_runs(),
        runs,
        "nobody else held the run, so it went back with the pending window"
    );

    // The slot is free again, and the window is not handed back a second
    // time when the client dies.
    assert_eq!(client.held(), 0);
    assert!(matches!(client.surface(id), Err(SurfaceServiceError::Gone)));
    drop(client);
    assert!(service.take_returned().is_empty());
}

/// A cancelled create frees its slot for the next one: an instance that
/// abandons creates without bound is never refused for slots it no
/// longer holds.
#[test]
fn a_cancelled_create_holds_no_slot() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, views) = (SurfaceOwnership::new(), ViewTable::new());
    for _ in 0..2 * super::MAX_INSTANCE_SURFACES {
        let pending = client
            .prepare(
                &service,
                &views,
                client_memory(),
                pages(1, 1),
                geometry(64, 64),
            )
            .expect("every earlier create was cancelled, so a slot is free");
        drop(pending);
    }
    assert_eq!(client.held(), 0);
    assert_eq!(
        service.take_returned().len(),
        2 * super::MAX_INSTANCE_SURFACES
    );
}

/// A span something else was shown over while the compositor was
/// answering is refused in the last step: nothing more is shown, and the
/// window goes to the supervisor, which tells the compositor it is gone.
#[test]
fn a_span_taken_while_the_compositor_answered_is_refused_in_the_last_step() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, mut views) = (SurfaceOwnership::new(), ViewTable::new());

    let first = client
        .prepare(
            &service,
            &views,
            client_memory(),
            pages(1, 1),
            geometry(64, 64),
        )
        .expect("the span is free");
    let second = client
        .prepare(
            &service,
            &views,
            client_memory(),
            pages(1, 1),
            geometry(64, 64),
        )
        .expect("nothing is shown at the span yet");
    let (first, _view) = client
        .show(&mut views, client_memory(), first)
        .expect("the span is free");
    let id = second.id();

    let refused = client.show(&mut views, client_memory(), second);
    assert!(matches!(
        refused,
        Err(SurfaceServiceError::BadSpan(ViewError::Overlap))
    ));
    assert_eq!(views.len(), 1, "only the first window is shown");
    assert!(service.lookup(id).is_none());
    let returned = service.take_returned();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].ids.as_slice(), [id].as_slice());
    assert_eq!(client.held(), 1);
    assert!(client.surface(first.id()).is_ok());
}

/// A client that drops a window reaches the compositor's supervisor
/// even with the compositor's bounded request queue full: the drop goes
/// on the unbounded return queue, never on the one a full desktop
/// refuses.
#[test]
fn a_dropped_window_reaches_the_supervisor_with_the_request_queue_full() {
    test_hooks::install();
    let service = SurfaceService::new();
    let (mut client, mut views) = (SurfaceOwnership::new(), ViewTable::new());
    let (requests, _inbox) =
        crate::component::provider_channel::<super::SurfaceRequest>(SURFACE_REQUEST_QUEUE_DEPTH);
    let pending = client
        .prepare(
            &service,
            &views,
            client_memory(),
            pages(1, 1),
            geometry(64, 64),
        )
        .expect("the span is free");
    let (shared, view) = client
        .show(&mut views, client_memory(), pending)
        .expect("the span is free");
    let id = shared.id();

    // A compositor that has stopped taking work: every slot of its
    // request queue is spoken for.
    let mut queued = 0;
    while requests
        .try_send(super::SurfaceRequest::Commit {
            id,
            region: SurfaceRect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            reply: futures::channel::oneshot::channel().0,
        })
        .is_ok()
    {
        queued += 1;
    }
    assert_eq!(queued, SURFACE_REQUEST_QUEUE_DEPTH);

    // What the resource's drop does.
    views.end(view).expect("the test pool refills anything");
    client.release(&service, id);

    assert!(!shared.is_alive());
    let returned = service.take_returned();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0].ids.as_slice(), [id].as_slice());
    assert_eq!(client.held(), 0);
}
