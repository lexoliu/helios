//! The display path's own tests.
//!
//! The device side is a recording fake: every call the owner task makes
//! is appended to a log, so a test asserts on the exact sequence the
//! display engine was asked for rather than on what the kernel intended.
//! The address space is the recording surface `device::platform`'s test
//! hooks install, so a test can also see when a frame buffer's pages
//! were committed and when they went back.

use alloc::vec::Vec;
use core::future::Future;
use core::pin::pin;

use futures::channel::oneshot;
use futures_lite::future::block_on;
use helios_hal::display::{
    CursorImage, DisplayDevice, DisplayMode, DisplayResult, FramebufferId, PixelFormat, Point,
    Rect, ScanoutId, ScanoutInfo, ScanoutList,
};
use helios_hal::pmm::PhysFrameRange;
use helios_hal::vmm::VirtAddr;
use std::sync::Mutex;
use triomphe::Arc;

use crate::component::ProviderReceiver;
use crate::device::{DISPLAY_WINDOW_BYTES, DeviceWindow, test_hooks};
use crate::test_support::TestCpu;

use super::owner::{display_channels, serve_control};
use super::service::{ControlRequest, DisplayService, DisplayShared};
use super::{DisplayOwnership, DisplayServiceError};

/// The one linear-memory reservation the kernel builds, and where the
/// tests pretend it sits.
const RESERVATION_BYTES: u64 = 1 << 32;
const MEMORY_BASE: usize = 0x1_0000_0000;

/// A mode small enough that several fit the window, and whose frame is a
/// whole number of pages.
const SMALL: DisplayMode = DisplayMode::new(64, 64);

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

/// A display engine that records what it was asked and always agrees.
struct FakeDisplay {
    calls: Mutex<Vec<Call>>,
    next: Mutex<u32>,
}

impl FakeDisplay {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            next: Mutex::new(1),
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
        Ok(id)
    }

    async fn destroy_framebuffer(&self, framebuffer: FramebufferId) -> DisplayResult<()> {
        self.record(Call::Destroy(framebuffer));
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

/// The display window of an instance whose memory sits at [`MEMORY_BASE`].
fn window() -> DeviceWindow {
    DeviceWindow::top_of(VirtAddr::new(MEMORY_BASE), RESERVATION_BYTES).below(DISPLAY_WINDOW_BYTES)
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

/// A claim, its arena, and the queue into the server.
fn claimed(shared: &Arc<DisplayShared>) -> DisplayOwnership {
    let service = DisplayService::from_shared(shared.clone());
    let mut ownership = DisplayOwnership::new();
    ownership
        .claim(&service, window())
        .expect("the display is free");
    ownership
}

/// Ask the server to create one surface for `ownership`.
async fn create_surface(ownership: &mut DisplayOwnership) -> FramebufferId {
    let generation = ownership
        .claim_ref()
        .expect("the claim is held")
        .generation();
    let sender = ownership.claim_ref().expect("the claim is held").sender();
    let frame = ownership
        .pin_frame(SMALL, PixelFormat::Bgrx8888.bytes_per_pixel())
        .expect("the window has room");
    let (reply, answer) = oneshot::channel();
    sender
        .control(
            ControlRequest::CreateSurface {
                generation,
                scanout: ScanoutId::new(0),
                mode: SMALL,
                format: PixelFormat::Bgrx8888,
                backing: frame.backing,
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

    let first = service.claim().expect("the display is free");
    assert_eq!(
        service.claim().err(),
        Some(DisplayServiceError::AlreadyClaimed)
    );

    // Letting go is not enough. Until the owner task has taken the
    // resources back the display is neither held nor free, and handing
    // it out would put somebody else's pixels on a screen the next
    // compositor thinks is blank.
    drop(first);
    assert_eq!(
        service.claim().err(),
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
        .claim(&service, window())
        .expect("the display is free");
    assert_eq!(
        ownership.claim(&service, window()).err(),
        Some(DisplayServiceError::AlreadyClaimed)
    );
}

/// The frame buffer is the instance's own memory: committed from its
/// pool, counted against it, and inside its own window.
#[test]
fn a_frame_buffer_is_pinned_in_the_instance_s_own_window() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let mut ownership = claimed(&shared);
    let before = test_hooks::shootdowns();

    let framebuffer = with_server(&FakeDisplay::new(), &shared, &control, async {
        create_surface(&mut ownership).await
    });

    let frame_bytes = SMALL
        .frame_bytes(PixelFormat::Bgrx8888)
        .expect("a 64 by 64 frame fits") as u64;
    assert_eq!(framebuffer, FramebufferId::new(1));
    assert_eq!(ownership.pinned_bytes(), frame_bytes);
    assert_eq!(
        test_hooks::shootdowns() - before,
        1,
        "one contiguous commit, one shootdown"
    );
    let placement = window();
    assert!(
        ownership.window().expect("a claim has a window").offset() == placement.offset(),
        "the frame buffer's window is the one the instance was given"
    );
}

/// The window is a bound, and reaching it is answered rather than
/// quietly reusing memory the display engine may still be reading.
#[test]
fn a_frame_buffer_larger_than_the_window_is_refused() {
    test_hooks::install();
    let (shared, _control, _cursor) = display_channels();
    let mut ownership = claimed(&shared);

    // A mode whose frame is larger than the whole display window.
    let huge = DisplayMode::new(16384, 16384);
    assert_eq!(
        ownership
            .pin_frame(huge, PixelFormat::Bgrx8888.bytes_per_pixel())
            .err(),
        Some(DisplayServiceError::WindowExhausted)
    );
    assert_eq!(ownership.pinned_bytes(), 0);
}

/// A surface reaches its scanout through create-then-attach, and a
/// present is one flush of the rectangle that changed.
#[test]
fn a_surface_is_attached_when_it_is_created_and_a_present_flushes_it() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let mut ownership = claimed(&shared);

    let region = Rect::of(SMALL);
    with_server(&device, &shared, &control, async {
        let framebuffer = create_surface(&mut ownership).await;
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
/// pages come back only after that: a device still reading a resource
/// whose pages had gone back to a pool would put another instance's
/// memory on the screen.
#[test]
fn a_released_claim_blanks_before_it_destroys_and_frees_after() {
    test_hooks::install();
    let (shared, control, _cursor) = display_channels();
    let device = FakeDisplay::new();
    let mut ownership = claimed(&shared);
    let frame_bytes = SMALL
        .frame_bytes(PixelFormat::Bgrx8888)
        .expect("a 64 by 64 frame fits") as u64;

    // One server for the whole scenario: what has to be given back is
    // the owner task's own record of what it created, and a second
    // server would start with an empty one.
    with_server(&device, &shared, &control, async {
        create_surface(&mut ownership).await;
        assert_eq!(ownership.pinned_bytes(), frame_bytes);
        let committed = test_hooks::changes().len();

        // Killing the instance is dropping its store, which is this.
        ownership.release();
        assert_eq!(
            test_hooks::changes().len(),
            committed,
            "the pages are not freed by the drop itself"
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
            Some(test_hooks::MappingChange::Released(_))
        ),
        "the pages go back once the display engine has let go"
    );

    let service = DisplayService::from_shared(shared.clone());
    assert!(
        service.claim().is_ok(),
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
    let frame = ownership
        .pin_frame(SMALL, PixelFormat::Bgrx8888.bytes_per_pixel())
        .expect("the window has room");

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
                    backing: frame.backing,
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
