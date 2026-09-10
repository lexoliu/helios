//! The kernel's ownership of the machine's display device.
//!
//! A display device is one of the few pieces of hardware whose state
//! changes without anybody asking: a monitor is plugged in, unplugged,
//! or resized by whatever is hosting this machine, and the device
//! announces it through a configuration-change interrupt. The
//! announcement has to be consumed by somebody — a driver that nobody
//! reads leaves the event latched and the *next* change raises no
//! interrupt at all — so the kernel owns the device and follows its
//! topology for as long as the machine runs.
//!
//! What is displayed is decided by whichever instance holds the claim.
//! It never touches the device: it asks the tasks here, and they are the
//! only code that speaks to the display engine.
//!
//! # SMP contract
//!
//! Three tasks, all local to the processor that brought the device up,
//! because that is the processor the device's interrupt is routed to.
//!
//! * The topology follower parks on the device's own change
//!   notification and never polls.
//! * The control server owns everything that goes up the device's
//!   control queue: display info, resources, frames. It serves one
//!   request at a time, so frames on one display are published in the
//!   order they were asked for.
//! * The cursor server owns the device's cursor queue. It is a separate
//!   task for the reason the hardware has a separate queue: moving the
//!   pointer must not wait behind a frame somebody is presenting.
//!
//! All three hold the same device handle. The trait's own contract says
//! every method takes `&self`, may be called from several tasks at once,
//! and that an implementation serialises access to its own rings.

use arrayvec::ArrayVec;
use core::pin::pin;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use futures::future::{Either, select};
use helios_hal::cpu::Cpu;
use helios_hal::display::{
    DisplayDevice, DisplayError, FramebufferId, MAX_SCANOUTS, Rect, ScanoutId,
};
use helios_hal::watchdog::Watchdog;
use triomphe::Arc;

use crate::Kernel;
use crate::component::{ProviderReceiver, provider_channel};
use crate::exec::monotonic_nanos;

use super::DisplayServiceError;
use super::service::{
    ClaimState, ControlRequest, CursorRequest, DisplayService, DisplayShared, FrameToken,
    REQUEST_QUEUE_DEPTH,
};
use crate::pins::MAX_PINNED_FRAMES;

/// Brings the machine's display under kernel ownership and publishes the
/// service `helios:system/display` is served from.
///
/// The device is never handed anywhere else. What callers get back is a
/// handle to the tasks this spawns, which is what a claim and every
/// frame after it travels through.
pub fn install_display_device<CpuImpl, WatchdogImpl, Device>(
    kernel: &Kernel<CpuImpl, WatchdogImpl>,
    cpu: &CpuImpl,
    device: Device,
) -> DisplayService
where
    CpuImpl: Cpu + Clone + Send + Sync + 'static,
    WatchdogImpl: Watchdog + Clone,
    Device: DisplayDevice + Clone,
{
    let (shared, control_rx, cursor_rx) = display_channels();

    {
        let device = device.clone();
        let shared = shared.clone();
        let cpu = cpu.clone();
        kernel.spawn_local_detached(async move {
            follow_display_topology(&device, &shared, &cpu).await;
        });
    }
    {
        let device = device.clone();
        let shared = shared.clone();
        let cpu = cpu.clone();
        kernel.spawn_local_detached(async move {
            serve_control(&device, &shared, &control_rx, &cpu).await;
        });
    }
    {
        let device = device.clone();
        let shared = shared.clone();
        kernel.spawn_local_detached(async move {
            serve_cursor(&device, &shared, &cursor_rx).await;
        });
    }

    DisplayService::from_shared(shared)
}

/// The shared state and the two inboxes one display's tasks are built
/// around.
///
/// Split out of [`install_display_device`] so a test can drive the same
/// server loops without a kernel to spawn them on.
pub(super) fn display_channels() -> (
    Arc<DisplayShared>,
    ProviderReceiver<ControlRequest>,
    ProviderReceiver<CursorRequest>,
) {
    let (control, control_rx) = provider_channel(REQUEST_QUEUE_DEPTH);
    let (cursor, cursor_rx) = provider_channel(REQUEST_QUEUE_DEPTH);
    let shared = Arc::new(DisplayShared {
        control,
        cursor,
        claim: AtomicU8::new(ClaimState::FREE),
        generation: AtomicU64::new(0),
        release: crate::exec::Notify::new(),
        changes: Arc::new(super::SequenceSignal::new()),
        scanout_count: AtomicU32::new(0),
        returned: concurrent_queue::ConcurrentQueue::bounded(1),
    });
    (shared, control_rx, cursor_rx)
}

/// Follows the device's topology for as long as the machine runs.
///
/// The task is what keeps the device's change notification live: each
/// announcement is collected and the new set of scanouts read back, so
/// the device is never left with an event nobody took.
pub(super) async fn follow_display_topology<Device, CpuImpl>(
    device: &Device,
    shared: &DisplayShared,
    cpu: &CpuImpl,
) where
    Device: DisplayDevice,
    CpuImpl: Cpu,
{
    match device.scanouts().await {
        Ok(scanouts) => {
            shared
                .scanout_count
                .store(scanouts.len() as u32, Ordering::Release);
            // The bring-up line every other device prints. It is also
            // the only evidence that the display's own tasks are being
            // driven at all: everything else they do is answered to a
            // caller rather than logged.
            tracing::info!(
                scanouts = scanouts.len(),
                enabled = scanouts.iter().filter(|info| info.enabled).count(),
                "display service online"
            );
        }
        Err(error) => tracing::warn!(
            %error,
            "the display device would not report its topology at bring-up"
        ),
    }
    loop {
        device.display_changed().await;
        match device.scanouts().await {
            Ok(scanouts) => {
                let enabled = scanouts.iter().filter(|info| info.enabled).count();
                shared
                    .scanout_count
                    .store(scanouts.len() as u32, Ordering::Release);
                shared.changes.bump(monotonic_nanos(cpu));
                tracing::info!(
                    scanouts = scanouts.len(),
                    enabled,
                    "display topology changed"
                );
                for info in &scanouts {
                    tracing::debug!(
                        scanout = info.id.index(),
                        x = info.geometry.x,
                        y = info.geometry.y,
                        width = info.geometry.width,
                        height = info.geometry.height,
                        enabled = info.enabled,
                        "scanout geometry"
                    );
                }
            }
            Err(error) => {
                // The device announced a change and then refused to
                // describe it. The topology the kernel holds is the one
                // it read last; the next announcement re-reads it.
                tracing::warn!(
                    %error,
                    "the display device would not report its topology after announcing a change"
                );
            }
        }
    }
}

/// One frame buffer the current claim created.
struct SurfaceRecord {
    framebuffer: FramebufferId,
    /// The output it was attached to, absent for a cursor plane's own
    /// resource, which no scanout latches.
    scanout: Option<ScanoutId>,
    /// How many frames it has published.
    sequence: u64,
}

/// Everything the current claim has the display engine holding.
///
/// The owner task, not the claiming store, is the record of what has to
/// be given back: a store that is killed mid-frame leaves nothing
/// behind, and its resources are still here to be released.
struct ClaimResources {
    generation: u64,
    surfaces: ArrayVec<SurfaceRecord, MAX_PINNED_FRAMES>,
    /// The outputs this claim pointed at something. Blanked before
    /// anything is destroyed, so the display engine is never reading a
    /// resource whose pages are on their way back to a pool.
    scanouts: ArrayVec<ScanoutId, MAX_SCANOUTS>,
}

impl ClaimResources {
    const fn new() -> Self {
        Self {
            generation: 0,
            surfaces: ArrayVec::new_const(),
            scanouts: ArrayVec::new_const(),
        }
    }

    fn record(&mut self, framebuffer: FramebufferId) -> Option<&mut SurfaceRecord> {
        self.surfaces
            .iter_mut()
            .find(|record| record.framebuffer == framebuffer)
    }
}

/// Serves the display engine's control queue.
pub(super) async fn serve_control<Device, CpuImpl>(
    device: &Device,
    shared: &DisplayShared,
    inbox: &ProviderReceiver<ControlRequest>,
    cpu: &CpuImpl,
) where
    Device: DisplayDevice,
    CpuImpl: Cpu,
{
    let mut held = ClaimResources::new();
    loop {
        // The release is a banked permit rather than a message, and it
        // is polled first: a claim that has been let go is torn down
        // before anything queued under it is served, and the generation
        // check below drops whatever was left in the queue.
        let released = pin!(shared.release.notified());
        let next = pin!(inbox.recv());
        match select(released, next).await {
            Either::Left(((), _)) => {
                release_claim(device, &mut held).await;
                // Only now: the display engine has stopped reading, so
                // dropping the arena hands the pages back to a pool
                // nothing is scanning out.
                while let Ok(pins) = shared.returned.pop() {
                    drop(pins);
                }
                shared.claim.store(ClaimState::FREE, Ordering::Release);
            }
            Either::Right((Some(request), _)) => {
                serve_control_request(device, shared, &mut held, request, cpu).await;
            }
            Either::Right((None, _)) => return,
        }
    }
}

async fn serve_control_request<Device, CpuImpl>(
    device: &Device,
    shared: &DisplayShared,
    held: &mut ClaimResources,
    request: ControlRequest,
    cpu: &CpuImpl,
) where
    Device: DisplayDevice,
    CpuImpl: Cpu,
{
    let generation = shared.generation.load(Ordering::Acquire);
    if request.generation() != generation {
        // A request that outlived its claim. Nothing is answered: the
        // instance that asked is gone, so the reply channel's other end
        // is gone with it, and serving the request against whoever holds
        // the display now would let a dead compositor draw on a live
        // one's screen.
        return;
    }
    if held.generation != generation {
        held.generation = generation;
    }
    match request {
        ControlRequest::Scanouts { reply, .. } => {
            let _ = reply.send(device.scanouts().await.map_err(DisplayServiceError::from));
        }
        ControlRequest::Modes { scanout, reply, .. } => {
            let _ = reply.send(
                device
                    .preferred_mode(scanout)
                    .await
                    .map_err(DisplayServiceError::from),
            );
        }
        ControlRequest::CreateSurface {
            scanout,
            mode,
            format,
            backing,
            reply,
            ..
        } => {
            let outcome = create_surface(device, held, scanout, mode, format, backing).await;
            let _ = reply.send(outcome);
        }
        ControlRequest::CreateCursor {
            format,
            backing,
            reply,
            ..
        } => {
            let outcome = create_cursor(device, held, format, backing).await;
            let _ = reply.send(outcome);
        }
        ControlRequest::DestroySurface { framebuffer, .. } => {
            destroy_surface(device, held, framebuffer).await;
        }
        ControlRequest::Present {
            framebuffer,
            region,
            vsync,
            reply,
            ..
        } => {
            let outcome = match device.flush(framebuffer, region).await {
                Ok(()) => {
                    let sequence = match held.record(framebuffer) {
                        Some(record) => {
                            record.sequence += 1;
                            record.sequence
                        }
                        None => 0,
                    };
                    let presented_nanos = monotonic_nanos(cpu);
                    vsync.publish(sequence, presented_nanos);
                    Ok(FrameToken {
                        sequence,
                        presented_nanos,
                    })
                }
                Err(error) => Err(DisplayServiceError::from(error)),
            };
            let _ = reply.send(outcome);
        }
        ControlRequest::SetCursor {
            scanout,
            position,
            image,
            reply,
            ..
        } => {
            let _ = reply.send(
                device
                    .set_cursor(scanout, position, image)
                    .await
                    .map_err(DisplayServiceError::from),
            );
        }
    }
}

async fn create_surface<Device: DisplayDevice>(
    device: &Device,
    held: &mut ClaimResources,
    scanout: ScanoutId,
    mode: helios_hal::display::DisplayMode,
    format: helios_hal::display::PixelFormat,
    backing: helios_hal::pmm::PhysFrameRange,
) -> Result<FramebufferId, DisplayServiceError> {
    if held.surfaces.is_full() {
        return Err(DisplayServiceError::TooManySurfaces);
    }
    let framebuffer = device
        .create_framebuffer(mode, format, &[backing])
        .await
        .map_err(DisplayServiceError::from)?;
    if let Err(error) = device
        .set_scanout(scanout, framebuffer, Rect::of(mode))
        .await
    {
        // The resource exists but nothing shows it. Handing the caller a
        // surface it could draw into and never see would be worse than
        // the refusal, so the resource goes back before the error does.
        if let Err(destroy) = device.destroy_framebuffer(framebuffer).await {
            tracing::warn!(
                %destroy,
                "a frame buffer the display engine would not release after a failed scanout"
            );
        }
        return Err(DisplayServiceError::from(error));
    }
    held.surfaces.push(SurfaceRecord {
        framebuffer,
        scanout: Some(scanout),
        sequence: 0,
    });
    if !held.scanouts.contains(&scanout) && !held.scanouts.is_full() {
        held.scanouts.push(scanout);
    }
    Ok(framebuffer)
}

async fn create_cursor<Device: DisplayDevice>(
    device: &Device,
    held: &mut ClaimResources,
    format: helios_hal::display::PixelFormat,
    backing: helios_hal::pmm::PhysFrameRange,
) -> Result<FramebufferId, DisplayServiceError> {
    if held.surfaces.is_full() {
        return Err(DisplayServiceError::TooManySurfaces);
    }
    let framebuffer = device
        .create_framebuffer(helios_hal::display::CursorImage::MODE, format, &[backing])
        .await
        .map_err(DisplayServiceError::from)?;
    held.surfaces.push(SurfaceRecord {
        framebuffer,
        scanout: None,
        sequence: 0,
    });
    Ok(framebuffer)
}

async fn destroy_surface<Device: DisplayDevice>(
    device: &Device,
    held: &mut ClaimResources,
    framebuffer: FramebufferId,
) {
    let Some(index) = held
        .surfaces
        .iter()
        .position(|record| record.framebuffer == framebuffer)
    else {
        return;
    };
    let record = held.surfaces.remove(index);
    if let Some(scanout) = record.scanout
        && let Err(error) = device.blank_scanout(scanout).await
    {
        tracing::warn!(
            %error,
            scanout = scanout.index(),
            "the display engine would not blank a scanout whose surface is going away"
        );
    }
    if let Err(error) = device.destroy_framebuffer(framebuffer).await {
        tracing::warn!(
            %error,
            "the display engine would not release a frame buffer its owner destroyed"
        );
    }
}

/// Hand everything the claim held back to the machine.
///
/// The order is what makes it safe to release the caller's pages
/// afterwards: the pointer comes off, every output stops latching, and
/// only then is a resource dropped. A device still reading a resource
/// whose pages had gone back to a pool would put another instance's
/// memory on the screen.
async fn release_claim<Device: DisplayDevice>(device: &Device, held: &mut ClaimResources) {
    for scanout in &held.scanouts {
        if let Err(error) = device.hide_cursor(*scanout).await {
            tracing::warn!(
                %error,
                scanout = scanout.index(),
                "the display engine would not take the pointer off a released scanout"
            );
        }
        if let Err(error) = device.blank_scanout(*scanout).await {
            tracing::warn!(
                %error,
                scanout = scanout.index(),
                "the display engine would not blank a released scanout"
            );
        }
    }
    for record in &held.surfaces {
        if let Err(error) = device.destroy_framebuffer(record.framebuffer).await {
            tracing::warn!(
                %error,
                "the display engine would not release a frame buffer of a claim that ended"
            );
        }
    }
    tracing::info!(
        surfaces = held.surfaces.len(),
        scanouts = held.scanouts.len(),
        "display claim released"
    );
    held.surfaces.clear();
    held.scanouts.clear();
}

/// Serves the display engine's cursor queue.
pub(super) async fn serve_cursor<Device: DisplayDevice>(
    device: &Device,
    shared: &DisplayShared,
    inbox: &ProviderReceiver<CursorRequest>,
) {
    while let Some(request) = inbox.recv().await {
        if request.generation() != shared.generation.load(Ordering::Acquire) {
            continue;
        }
        match request {
            CursorRequest::Move {
                scanout,
                position,
                reply,
                ..
            } => {
                let _ = reply.send(
                    device
                        .move_cursor(scanout, position)
                        .await
                        .map_err(DisplayServiceError::from),
                );
            }
            CursorRequest::Hide { scanout, reply, .. } => {
                let _ = reply.send(
                    device
                        .hide_cursor(scanout)
                        .await
                        .map_err(DisplayServiceError::from),
                );
            }
        }
    }
}

impl From<DisplayError> for DisplayServiceError {
    fn from(error: DisplayError) -> Self {
        match error {
            DisplayError::OutOfMemory => Self::OutOfMemory,
            DisplayError::UnknownScanout(scanout) => Self::NoSuchScanout(scanout.index()),
            DisplayError::InvalidParameter | DisplayError::CursorSize { .. } => {
                Self::UnsupportedMode
            }
            DisplayError::RegionOutOfBounds { .. } => Self::OutOfBounds,
            // Everything left is the display engine failing rather than
            // refusing: a resource the kernel's own bookkeeping named
            // and the device does not have, a backing the kernel built
            // wrong, a code that belongs to no request, a transport
            // fault. A compositor cannot do anything about any of them
            // beyond giving the display back.
            DisplayError::Unspecified
            | DisplayError::UnknownFramebuffer(_)
            | DisplayError::UnexpectedResponse { .. }
            | DisplayError::TooManyBackingRanges { .. }
            | DisplayError::TooManyFramebuffers { .. }
            | DisplayError::Transport(_) => Self::DeviceFault,
        }
    }
}
