//! The display path's own tests.
//!
//! The device side is a recording fake: every call the owner task makes
//! is appended to a log, so a test asserts on the exact sequence the
//! display engine was asked for rather than on what the kernel intended.
//! The address space is the recording surface `device::platform`'s test
//! hooks install, so a test can also see when a frame buffer's run was
//! taken and when it went back.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;

use futures::channel::oneshot;
use futures_lite::future::block_on;
use helios_hal::display::{
    CursorImage, DisplayDevice, DisplayError, DisplayMode, DisplayResult, FramebufferId,
    PixelFormat, Point, Rect, ScanoutId, ScanoutInfo, ScanoutList,
};
use helios_hal::pmm::PhysFrameRange;
use std::sync::Mutex;
use triomphe::Arc;

use crate::buffer::Buffer;
use crate::component::ProviderReceiver;
use crate::device::test_hooks;
use crate::process::test_caps;
use crate::test_support::TestCpu;

use super::owner::{display_channels, serve_control};
use super::service::{ControlRequest, DisplayService, DisplayShared};
use super::{DisplayOwnership, DisplayServiceError};

/// A mode whose frame is a whole number of pages.
const SMALL: DisplayMode = DisplayMode::new(64, 64);

/// A frame buffer for one [`SMALL`] frame, as `create` pins it.
fn small_frame() -> Buffer {
    let bytes = SMALL
        .frame_bytes(PixelFormat::Bgrx8888)
        .expect("a 64 by 64 frame fits") as u64;
    Buffer::pinned(bytes).expect("the test pool has room for one frame")
}

/// One thing the display engine was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    Create(FramebufferId, DisplayMode),
    SetScanout(ScanoutId, FramebufferId),
    Blank(ScanoutId),
    Flush(FramebufferId, Rect),
    SetCursor(ScanoutId, FramebufferId),
    HideCursor(ScanoutId),
    MoveCursor(ScanoutId, Point),
    Destroy(FramebufferId),
}

/// The receiving end of a `CreateSurface` reply.
type CreateAnswer = oneshot::Receiver<Result<FramebufferId, DisplayServiceError>>;

/// A display engine that records what it was asked and agrees, unless a
/// test made it refuse to destroy.
struct FakeDisplay {
    calls: Mutex<Vec<Call>>,
    next: Mutex<u32>,
    /// Answer every `destroy_framebuffer` with a fault, as an engine
    /// that may still be latching the resource would.
    refuse_destroy: bool,
    /// A caller's end of the reply, dropped while the next resource is
    /// being created: the caller stopped waiting mid-create.
    abandon_on_create: Mutex<Option<CreateAnswer>>,
}

impl FakeDisplay {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next: Mutex::new(1),
            refuse_destroy: false,
            abandon_on_create: Mutex::new(None),
        }
    }

    fn refusing_destroy() -> Self {
        Self {
            refuse_destroy: true,
            ..Self::new()
        }
    }

    fn record(&self, call: Call) {
        self.calls.lock().expect("no test panics here").push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("no test panics here").clone()
    }
}

impl DisplayDevice for FakeDisplay {
    async fn scanouts(&self) -> DisplayResult<ScanoutList> {
        let mut list = ScanoutList::new();
        list.push(ScanoutInfo {
            id: ScanoutId::new(0),
            geometry: Rect::new(0, 0, SMALL.width, SMALL.height),
            enabled: true,
        });
        Ok(list)
    }

    async fn preferred_mode(&self, _scanout: ScanoutId) -> DisplayResult<DisplayMode> {
        Ok(SMALL)
    }

    async fn create_framebuffer(
        &self,
        mode: DisplayMode,
        _format: PixelFormat,
        _backing: &[PhysFrameRange],
    ) -> DisplayResult<FramebufferId> {
        let id = {
            let mut next = self.next.lock().expect("no test panics here");
            let id = FramebufferId::new(*next);
            *next += 1;
            id
        };
        self.record(Call::Create(id, mode));
        drop(
            self.abandon_on_create
                .lock()
                .expect("no test panics here")
                .take(),
        );
        Ok(id)
    }

    async fn destroy_framebuffer(&self, framebuffer: FramebufferId) -> DisplayResult<()> {
        self.record(Call::Destroy(framebuffer));
        if self.refuse_destroy {
            return Err(DisplayError::Unspecified);
        }
        Ok(())
    }

    async fn set_scanout(
        &self,
        scanout: ScanoutId,
        framebuffer: FramebufferId,
        _source: Rect,
    ) -> DisplayResult<()> {
        self.record(Call::SetScanout(scanout, framebuffer));
        Ok(())
    }

    async fn blank_scanout(&self, scanout: ScanoutId) -> DisplayResult<()> {
        self.record(Call::Blank(scanout));
        Ok(())
    }

    async fn flush(&self, framebuffer: FramebufferId, region: Rect) -> DisplayResult<()> {
        self.record(Call::Flush(framebuffer, region));
        Ok(())
    }

    async fn set_cursor(
        &self,
        scanout: ScanoutId,
        _position: Point,
        image: CursorImage,
    ) -> DisplayResult<()> {
        self.record(Call::SetCursor(scanout, image.framebuffer));
        Ok(())
    }

    async fn hide_cursor(&self, scanout: ScanoutId) -> DisplayResult<()> {
        self.record(Call::HideCursor(scanout));
        Ok(())
    }

    async fn move_cursor(&self, scanout: ScanoutId, position: Point) -> DisplayResult<()> {
        self.record(Call::MoveCursor(scanout, position));
        Ok(())
    }

    async fn display_changed(&self) {
        // Nothing announces a change in these tests, and a fake that
        // resolved immediately would spin the topology follower.
        core::future::pending::<()>().await;
    }
}

/// Run `work` with the control server running beside it.
///
/// The server never returns, so the result is the work's; what the
/// server is there for is to answer the requests the work makes.
fn with_server<T>(
    device: &FakeDisplay,
    shared: &DisplayShared,
    inbox: &ProviderReceiver<ControlRequest>,
    work: impl Future<Output = T>,
) -> T {
    let cpu = TestCpu::without_entropy();
    block_on(async {
        let work = pin!(work);
        let server = pin!(serve_control(device, shared, inbox, &cpu));
        futures_lite::future::or(async { Some(work.await) }, async {
            server.await;
            None
        })
        .await
        .expect("the control server does not end on its own")
    })
}

/// A claim and the queue into the server.
fn claimed(shared: &Arc<DisplayShared>) -> DisplayOwnership {
    let service = DisplayService::from_shared(shared.clone());
    let mut ownership = DisplayOwnership::new();
    ownership
        .claim(&service, test_caps::display())
        .expect("the display is free");
    ownership
}

/// Ask the server to create one surface for `ownership`, handing it the
/// only reference to a fresh frame buffer.
async fn create_surface(ownership: &DisplayOwnership) -> FramebufferId {
    let generation = ownership
        .claim_ref()
        .expect("the claim is held")
        .generation();
    let sender = ownership.claim_ref().expect("the claim is held").sender();
    let (reply, answer) = oneshot::channel();
    sender
        .control(
            ControlRequest::CreateSurface {
                generation,
                scanout: ScanoutId::new(0),
                mode: SMALL,
                format: PixelFormat::Bgrx8888,
                backing: small_frame(),
                reply,
            },
            answer,
        )
        .await
        .expect("the fake display agrees")
}

/// A second instance is refused rather than queued: a compositor waiting
/// for a display another compositor holds is a provisioning mistake.
#[test]
fn a_second_instance_is_refused_the_display_the_first_holds() {
    test_hooks::install();
    let (shared, _control, _cursor) = display_channels();
    let service = DisplayService::from_shared(shared.clone());

    let first = service
        .claim(test_caps::display())
        .expect("the display is free");
    assert_eq!(
        service.claim(test_caps::display()).err(),
        Some(DisplayServiceError::AlreadyClaimed)
    );

    // Letting go is not enough. Until the owner task has taken the
    // resources back the display is neither held nor free, and handing
    // it out would put somebody else's pixels on a screen the next
    // compositor thinks is blank.
    drop(first);
    assert_eq!(
        service.claim(test_caps::display()).err(),
        Some(DisplayServiceError::AlreadyClaimed)
    );
}

/// One instance holds one display: a second claim from the same store
/// would make the release ambiguous.
#[test]
fn one_instance_holds_the_display_once() {
    test_hooks::install();
    let (shared, _control, _cursor) = display_channels();
    let service = DisplayService::from_shared(shared.clone());
    let mut ownership = DisplayOwnership::new();

    ownership
        .claim(&service, test_caps::display())
        .expect("the display is free");
    assert_eq!(
        ownership.claim(&service, test_caps::display()).err(),
        Some(DisplayServiceError::AlreadyClaimed)
    );
}

/// The display engine's record of a resource holds its frame buffer: a
/// run handed over with its only reference stays pinned for as long as
/// the resource exists, and creating it changes no translation.
#[test]
fn the_display_engine_s_record_keeps_a_frame_buffer_pinned() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let ownership = claimed(&shared);
    let (runs, shootdowns) = (test_hooks::live_runs(), test_hooks::shootdowns());

    // The record is the owner task's, so the run is counted while that
    // task is still alive.
    let framebuffer = with_server(&FakeDisplay::new(), &shared, &control, async {
        let framebuffer = create_surface(&ownership).await;
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the run outlives the caller's reference while the resource exists"
        );
        framebuffer
    });

    assert_eq!(framebuffer, FramebufferId::new(1));
    assert_eq!(
        test_hooks::shootdowns(),
        shootdowns,
        "pinning a run maps nothing, so nothing is shot down"
    );
}

/// A mode the address space cannot describe is refused before anything
/// is pinned, as a mode no display engine drives.
#[test]
fn a_frame_buffer_no_address_space_could_describe_is_an_unsupported_mode() {
    test_hooks::install();
    let runs = test_hooks::live_runs();
    assert_eq!(
        Buffer::pinned(u64::MAX)
            .map_err(DisplayServiceError::from)
            .err(),
        Some(DisplayServiceError::UnsupportedMode)
    );
    assert_eq!(test_hooks::live_runs(), runs);
}

/// A surface reaches its scanout through create-then-attach, and a
/// present is one flush of the rectangle that changed.
#[test]
fn a_surface_is_attached_when_it_is_created_and_a_present_flushes_it() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let ownership = claimed(&shared);

    let region = Rect::of(SMALL);
    with_server(&device, &shared, &control, async {
        let framebuffer = create_surface(&ownership).await;
        let generation = ownership
            .claim_ref()
            .expect("the claim is held")
            .generation();
        let sender = ownership.claim_ref().expect("the claim is held").sender();
        let vsync = Arc::new(super::SequenceSignal::new());
        let (reply, answer) = oneshot::channel();
        let token = sender
            .control(
                ControlRequest::Present {
                    generation,
                    framebuffer,
                    region,
                    vsync: vsync.clone(),
                    reply,
                },
                answer,
            )
            .await
            .expect("the fake display agrees");
        assert_eq!(token.sequence, 1);
        assert_eq!(
            vsync.sequence(),
            1,
            "the vsync stream sees the same frame the future reports"
        );
    });

    assert_eq!(
        device.calls(),
        alloc::vec![
            Call::Create(FramebufferId::new(1), SMALL),
            Call::SetScanout(ScanoutId::new(0), FramebufferId::new(1)),
            Call::Flush(FramebufferId::new(1), region),
        ]
    );
}

/// Letting go blanks the outputs before it drops the resources, and the
/// run comes back only after that: a device still reading a resource
/// whose run had gone back to a pool would put another instance's
/// memory on the screen.
#[test]
fn a_released_claim_blanks_before_it_destroys_and_frees_after() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let mut ownership = claimed(&shared);
    let runs = test_hooks::live_runs();

    // One server for the whole scenario: what has to be given back is
    // the owner task's own record of what it created, and a second
    // server would start with an empty one.
    with_server(&device, &shared, &control, async {
        create_surface(&ownership).await;
        assert_eq!(test_hooks::live_runs() - runs, 1);

        // Killing the instance is dropping its store, which is this.
        ownership.release();
        assert_eq!(
            test_hooks::live_runs() - runs,
            1,
            "the run is not freed by the drop itself"
        );

        // The owner task takes it from here. The release is a banked
        // permit and the server takes it before anything in its inbox,
        // so one turn of the loop is enough.
        crate::yield_now().await;
        crate::yield_now().await;
    });

    let calls = device.calls();
    let blank = calls
        .iter()
        .position(|call| *call == Call::Blank(ScanoutId::new(0)))
        .expect("the output is blanked");
    let destroy = calls
        .iter()
        .position(|call| *call == Call::Destroy(FramebufferId::new(1)))
        .expect("the resource is destroyed");
    assert!(
        blank < destroy,
        "the output stops latching before the resource goes"
    );
    assert!(
        calls.contains(&Call::HideCursor(ScanoutId::new(0))),
        "the pointer comes off too"
    );
    assert!(
        matches!(
            test_hooks::changes().last(),
            Some(test_hooks::MappingChange::Freed(_))
        ),
        "the run goes back once the display engine has let go"
    );
    assert_eq!(test_hooks::live_runs(), runs);

    let service = DisplayService::from_shared(shared.clone());
    assert!(
        service.claim(test_caps::display()).is_ok(),
        "the display is free once its resources are back"
    );
}

/// A request made under a claim that has ended is dropped rather than
/// served: a dead compositor must not draw on a live one's screen.
#[test]
fn a_request_that_outlived_its_claim_is_not_served() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let mut ownership = claimed(&shared);
    let stale = ownership.claim_ref().expect("the claim is held").sender();
    let stale_generation = stale.generation();
    let frame = small_frame();

    with_server(&device, &shared, &control, async {
        // The first claim ends and a second one takes the display.
        ownership.release();
        crate::yield_now().await;
        crate::yield_now().await;
        let _second = claimed(&shared);

        let (reply, answer) = oneshot::channel();
        let refused = stale
            .control(
                ControlRequest::CreateSurface {
                    generation: stale_generation,
                    scanout: ScanoutId::new(0),
                    mode: SMALL,
                    format: PixelFormat::Bgrx8888,
                    backing: frame,
                    reply,
                },
                answer,
            )
            .await;
        assert_eq!(refused.err(), Some(DisplayServiceError::Closed));
    });

    assert!(
        !device
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Create(_, _))),
        "nothing was created for a claim that had ended"
    );
}

/// A request sitting in the inbox when its claim is let go is dropped
/// rather than served, however long the display then sits unclaimed: the
/// generation advances on release, not only on the next claim.
#[test]
fn a_request_queued_before_release_is_dropped() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let mut ownership = claimed(&shared);
    let claim = ownership.claim_ref().expect("the claim is held");
    let (generation, sender) = (claim.generation(), claim.sender());
    let runs = test_hooks::live_runs();

    // The request lands in the inbox before the server runs and before
    // the claim is let go; the reply stays open, so only the generation
    // can refuse it.
    let (reply, _answer) = oneshot::channel();
    block_on(sender.control_oneway(ControlRequest::CreateSurface {
        generation,
        scanout: ScanoutId::new(0),
        mode: SMALL,
        format: PixelFormat::Bgrx8888,
        backing: small_frame(),
        reply,
    }))
    .expect("the inbox has room");
    ownership.release();

    // No second claim: nothing moves the generation but the release.
    with_server(&device, &shared, &control, async {
        for _ in 0..3 {
            crate::yield_now().await;
        }
    });

    assert!(
        !device
            .calls()
            .iter()
            .any(|call| matches!(call, Call::Create(_, _) | Call::SetScanout(_, _))),
        "nothing was created or scanned out for a claim that had ended: {:?}",
        device.calls()
    );
    assert_eq!(
        test_hooks::live_runs(),
        runs,
        "the dropped request's frame goes back"
    );
}

/// A `create` its compositor stopped waiting for closes its end of the
/// reply as it goes. One that went before its request was served is
/// owed nothing and gets nothing made; one that went while the engine
/// was making the resource can never learn its identity, so the owner
/// task destroys it, and the run goes back.
#[test]
fn a_resource_made_for_a_caller_that_stopped_waiting_is_destroyed_by_the_owner() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let ownership = claimed(&shared);
    let claim = ownership.claim_ref().expect("the claim is held");
    let (generation, sender) = (claim.generation(), claim.sender());
    let runs = test_hooks::live_runs();
    let request = |reply| ControlRequest::CreateSurface {
        generation,
        scanout: ScanoutId::new(0),
        mode: SMALL,
        format: PixelFormat::Bgrx8888,
        backing: small_frame(),
        reply,
    };

    with_server(&device, &shared, &control, async {
        let (reply, answer) = oneshot::channel();
        drop(answer);
        sender
            .control_oneway(request(reply))
            .await
            .expect("the owner task is serving");
        for _ in 0..3 {
            crate::yield_now().await;
        }
        assert!(
            device.calls().is_empty(),
            "nothing is made for a caller that left before it was served"
        );

        let (reply, answer) = oneshot::channel();
        *device
            .abandon_on_create
            .lock()
            .expect("no test panics here") = Some(answer);
        sender
            .control_oneway(request(reply))
            .await
            .expect("the owner task is serving");
        for _ in 0..3 {
            crate::yield_now().await;
        }
    });

    let calls = device.calls();
    let made = FramebufferId::new(1);
    assert_eq!(calls.first(), Some(&Call::Create(made, SMALL)));
    assert!(
        calls.contains(&Call::Destroy(made)),
        "the resource nobody can name is destroyed: {calls:?}"
    );
    assert_eq!(
        test_hooks::live_runs(),
        runs,
        "its run goes back once the engine has let go"
    );
}

/// A resource the display engine would not destroy may still be latched,
/// so its run never goes back to the pool — whether it was destroyed on
/// its own or with the claim — and the device is not trusted into a
/// panic either.
#[test]
fn a_frame_buffer_the_engine_would_not_release_stays_pinned() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::refusing_destroy();
    let mut ownership = claimed(&shared);
    let runs = test_hooks::live_runs();

    with_server(&device, &shared, &control, async {
        let first = create_surface(&ownership).await;
        create_surface(&ownership).await;
        assert_eq!(test_hooks::live_runs() - runs, 2);

        let claim = ownership.claim_ref().expect("the claim is held");
        claim
            .sender()
            .control_oneway(ControlRequest::DestroySurface {
                generation: claim.generation(),
                framebuffer: first,
            })
            .await
            .expect("the owner task is serving");
        for _ in 0..3 {
            crate::yield_now().await;
        }
        assert_eq!(
            device.calls().last(),
            Some(&Call::Destroy(first)),
            "the surface's own destroy was served before the release"
        );
        ownership.release();
        for _ in 0..3 {
            crate::yield_now().await;
        }
    });

    let destroyed = device
        .calls()
        .iter()
        .filter(|call| matches!(call, Call::Destroy(_)))
        .count();
    assert_eq!(destroyed, 2, "both resources were asked to go");
    assert_eq!(
        test_hooks::live_runs() - runs,
        2,
        "neither run goes back to a pool the engine may still be reading"
    );
}

/// What a component sees when it calls `display.claim`: the claim is
/// answered by the instance's own authority before it reaches the
/// service.
#[cfg(feature = "wasmtime-runtime")]
mod component_host {
    use super::*;
    use crate::wasmtime_adapter::bindings::display::bindings::helios::system::display as contract;
    use crate::wasmtime_adapter::component_host::service::test_store;
    use crate::{DeviceClassRights, ProcessAuthority};

    fn authority_with(classes: DeviceClassRights) -> ProcessAuthority {
        let mut authority = ProcessAuthority::empty();
        authority.grant_device_classes(classes);
        authority
    }

    /// A store under `authority` on a machine whose display is
    /// `service`.
    fn store_under(
        authority: ProcessAuthority,
        service: &DisplayService,
    ) -> test_store::TestStoreData {
        test_store::store_data_authorized(authority, |runtime_state| {
            runtime_state.install_display_service(service.clone());
        })
    }

    /// An ordinary program is told what a machine with no display tells
    /// everyone, and the display stays free for its owner.
    #[test]
    fn ordinary_program_is_refused_the_display_as_if_there_were_none() {
        test_hooks::install();
        let (shared, _control, _cursor) = display_channels();
        let service = DisplayService::from_shared(shared.clone());

        for authority in [
            ProcessAuthority::empty(),
            ProcessAuthority::root(),
            authority_with(DeviceClassRights::all() - DeviceClassRights::DISPLAY),
        ] {
            let mut data = store_under(authority, &service);
            let answer = <test_store::TestStoreData as contract::Host>::claim(&mut data)
                .expect("a refusal is an answer, not a trap");
            assert_eq!(answer.err(), Some(contract::Error::Unavailable));
            assert!(!data.device.display().holds_display());
        }
        assert!(!service.is_claimed(), "nobody took the display");

        let mut absent = test_store::store_data_authorized(ProcessAuthority::empty(), |_| {});
        let none = <test_store::TestStoreData as contract::Host>::claim(&mut absent)
            .expect("a machine with no display answers");
        assert_eq!(
            none.err(),
            Some(contract::Error::Unavailable),
            "the answer is the one a machine with no display gives"
        );
    }

    /// The compositor's provisioning holds the display class, so its
    /// claim goes through.
    #[test]
    fn provisioned_owner_claims_the_display() {
        test_hooks::install();
        let (shared, _control, _cursor) = display_channels();
        let service = DisplayService::from_shared(shared.clone());
        let mut data = store_under(authority_with(DeviceClassRights::DISPLAY), &service);

        let answer = <test_store::TestStoreData as contract::Host>::claim(&mut data)
            .expect("the claim is answered");

        assert!(answer.is_ok(), "the owner is given the display");
        assert!(service.is_claimed());
        assert!(data.device.display().holds_display());
    }
}
