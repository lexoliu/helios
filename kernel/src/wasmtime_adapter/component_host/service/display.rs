//! `helios:system/display` for the component host.
//!
//! This is the whole of what a compositor sees of the machine's display,
//! and none of it is on the path a frame takes to the screen. A surface
//! is created once, its frame buffer shown at the span of its own linear
//! memory the compositor named; from then on the compositor writes
//! pixels there with ordinary stores, and `present` is one message to
//! the task that owns the display engine, which turns it into one copy
//! into the device's own copy of the resource and one flush.
//!
//! The frame buffer is shown in the last step of `create`, after the
//! display engine has answered and in the same step that records the
//! surface owning the view, so a `create` the compositor stops waiting
//! for shows nothing; the resource the engine made for it goes back
//! through a [`PendingReply`].
//!
//! The claim lives in the instance's store, so it is single-owned: the
//! resource handles a compositor holds carry the claim generation to
//! check against and nothing else. A handle that outlived a release then
//! names a display its instance no longer holds and is refused, rather
//! than being answered against whoever holds the display now.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the claim
//! and the store's view table need no lock, and every view is shown and
//! ended by that task alone. What is shared is the queue into the
//! owner tasks, which carries its own synchronisation, and the two
//! signals a stream is driven by — one per surface for frames, one per
//! device for topology — which are counted events whose readers arm the
//! notification before they read the count.

use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::display::{
    BlobId, BlobScanout, CursorImage, DisplayMode, FramebufferId, PixelFormat, Point, Rect,
    ScanoutId,
};
use triomphe::Arc;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, HasSelf, Linker, Resource, StreamProducer,
    StreamReader, StreamResult,
};

use crate::ComponentHostNetwork;
use crate::buffer::{Buffer, Span, ViewId};
use crate::display::{
    ControlRequest, CursorRequest, DisplaySender, DisplayServiceError, SequenceSignal,
    SurfaceSource,
};
use crate::gpu::ScanoutHold;
use crate::wasmtime_adapter::bindings::display::bindings::helios::system::display as display_wit;
use crate::wasmtime_adapter::component_host::BlobHandle;

use super::super::{PendingReply, StoreData, guest_memory, view_trap};

/// Register `helios:system/display` in a linker.
///
/// Every program linker gets it, for the reason the device interface
/// does: linkage is cached per binary while authority belongs to the
/// instance. The claim checks the instance's display device class, and
/// an instance without it is told `unavailable`, which is what a
/// machine with no display device tells every program.
pub(in crate::wasmtime_adapter::component_host) fn add_display_to_linker<
    CpuImpl,
    Net,
    StorageImpl,
>(
    linker: &mut Linker<StoreData<CpuImpl, Net, StorageImpl>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    display_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>(
        linker,
        |state| state,
    )
}

/// A compositor's hold on the display, as its store records it.
///
/// It carries the generation and nothing else: the claim itself is in
/// the store's [`crate::DisplayOwnership`], which is what the store's
/// own drop releases.
pub struct DisplayHandle {
    generation: u64,
}

impl DisplayHandle {
    pub const fn new(generation: u64) -> Self {
        Self { generation }
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }
}

/// One surface, as its store records it.
pub struct SurfaceHandle {
    generation: u64,
    scanout: ScanoutId,
    mode: DisplayMode,
    format: PixelFormat,
    /// What the scanout reads: a frame buffer this surface made, or a
    /// renderer's blob.
    source: SurfaceSource,
    /// The frame buffer's view in the compositor's memory, ended when
    /// the surface is dropped. A blob surface shows nothing: the blob's
    /// bytes are wherever the gpu path put them.
    view: Option<ViewId>,
    /// The cursor plane's own resource and frame buffer, once this
    /// surface has been given a pointer image. A compositor that never
    /// sets one pays nothing for the plane existing. The buffer is shown
    /// nowhere: the kernel writes each image through its own alias.
    cursor: Option<(FramebufferId, Buffer)>,
    /// Every frame this surface has published.
    vsync: Arc<SequenceSignal>,
}

fn to_wit_error(error: DisplayServiceError) -> display_wit::Error {
    match error {
        DisplayServiceError::Unavailable => display_wit::Error::Unavailable,
        DisplayServiceError::AlreadyClaimed => display_wit::Error::AlreadyClaimed,
        DisplayServiceError::NotClaimed => display_wit::Error::NotClaimed,
        DisplayServiceError::NoSuchScanout(_) => display_wit::Error::NoSuchScanout,
        DisplayServiceError::UnsupportedMode => display_wit::Error::UnsupportedMode,
        DisplayServiceError::OutOfBounds => display_wit::Error::OutOfBounds,
        DisplayServiceError::TooManySurfaces => display_wit::Error::TooManySurfaces,
        DisplayServiceError::BadSpan(_) => display_wit::Error::BadSpan,
        DisplayServiceError::OutOfMemory => display_wit::Error::OutOfMemory,
        DisplayServiceError::NoBlobScanout => display_wit::Error::NoBlobScanout,
        DisplayServiceError::StaleBlob => display_wit::Error::StaleBlob,
        // A display owner that has stopped serving is a machine on its
        // way down. There is nothing a compositor can do about it that
        // it would not also do about a device fault.
        DisplayServiceError::DeviceFault | DisplayServiceError::Closed => {
            display_wit::Error::DeviceFault
        }
    }
}

const fn to_wit_rect(rect: Rect) -> display_wit::Rect {
    display_wit::Rect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

const fn from_wit_rect(rect: display_wit::Rect) -> Rect {
    Rect::new(rect.x, rect.y, rect.width, rect.height)
}

const fn from_wit_point(point: display_wit::Point) -> Point {
    Point::new(point.x, point.y)
}

const fn to_wit_mode(mode: DisplayMode) -> display_wit::Mode {
    display_wit::Mode {
        width: mode.width,
        height: mode.height,
    }
}

const fn from_wit_mode(mode: display_wit::Mode) -> DisplayMode {
    DisplayMode::new(mode.width, mode.height)
}

const fn from_wit_format(format: display_wit::PixelFormat) -> PixelFormat {
    match format {
        display_wit::PixelFormat::Bgrx8888 => PixelFormat::Bgrx8888,
        display_wit::PixelFormat::Bgra8888 => PixelFormat::Bgra8888,
        display_wit::PixelFormat::Xrgb8888 => PixelFormat::Xrgb8888,
        display_wit::PixelFormat::Argb8888 => PixelFormat::Argb8888,
        display_wit::PixelFormat::Rgbx8888 => PixelFormat::Rgbx8888,
        display_wit::PixelFormat::Rgba8888 => PixelFormat::Rgba8888,
        display_wit::PixelFormat::Xbgr8888 => PixelFormat::Xbgr8888,
        display_wit::PixelFormat::Abgr8888 => PixelFormat::Abgr8888,
    }
}

impl<CpuImpl, Net, StorageImpl> StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    /// The queue into the display owner, checked against `generation`.
    fn display_sender(&self, generation: u64) -> Result<DisplaySender, DisplayServiceError> {
        let claim = self.device.display().claim_ref()?;
        if claim.generation() != generation {
            return Err(DisplayServiceError::NotClaimed);
        }
        Ok(claim.sender())
    }
}

/// Pin a fresh frame buffer of `bytes` for showing at `span` of the
/// calling instance's memory, once the display engine has a resource
/// for it.
///
/// The span is checked before anything is pinned, so a span the table
/// would refuse never costs the instance a run, and before the owner
/// task is told anything. The run is pinned from the calling instance's
/// own pool, inside its host call. Nothing is shown here.
fn prepare_frame<U, CpuImpl, Net, StorageImpl>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>,
    span: Span,
    bytes: u64,
) -> Result<(Buffer, crate::InstanceSpawner<CpuImpl>), DisplayServiceError>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let linear_memory = access.get().linear_memory;
    let memory = guest_memory(linear_memory, &access)?;
    let data = access.get();
    data.views.validate(memory, span, bytes)?;
    let buffer = Buffer::pinned(bytes)?;
    Ok((buffer, data.spawner().clone()))
}

/// Show a frame buffer the display engine took at `span` of the calling
/// instance's memory, and record the surface that owns the view, in one
/// step.
///
/// The span is checked again: another call may have shown something
/// overlapping it while the engine was creating the resource.
fn show_frame<U, CpuImpl, Net, StorageImpl>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>,
    span: Span,
    buffer: &Buffer,
    surface: impl FnOnce(ViewId) -> SurfaceHandle,
) -> wasmtime::Result<Result<Resource<SurfaceHandle>, DisplayServiceError>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let linear_memory = access.get().linear_memory;
    // The store's memory handle is recorded once, when the instance is
    // built, and `prepare_frame` already read it.
    let memory = guest_memory(linear_memory, &access)
        .expect("a store whose memory took the span when the frame was pinned has one");
    let data = access.get();
    let view = match data.views.show_buffer(memory, span, buffer, None) {
        Ok(view) => view,
        Err(error) => return Ok(Err(error.into())),
    };
    Ok(Ok(data.table.push(surface(view))?))
}

/// Tell the owner task to let go of resources a surface no longer
/// holds, without the store and without waiting.
///
/// A resource drop is synchronous and the display engine is not, so
/// the work is handed to a task rather than awaited. It cannot be lost
/// for good: if the task cannot be spawned, or the instance dies before
/// it runs, the claim's own release destroys the same resources.
fn spawn_display_cleanup<CpuImpl>(
    spawner: &crate::InstanceSpawner<CpuImpl>,
    sender: DisplaySender,
    generation: u64,
    surfaces: [Option<SurfaceSource>; 2],
) where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
{
    let spawned = spawner.try_spawn_detached(async move {
        for surface in surfaces.into_iter().flatten() {
            if let Err(error) = sender
                .control_oneway(ControlRequest::DestroySurface {
                    generation,
                    surface,
                })
                .await
            {
                tracing::warn!(
                    target: "helios_kernel::display",
                    %error,
                    "a dropped surface could not be handed back to the display owner"
                );
            }
        }
    });
    if let Err(error) = spawned {
        // The task arena is full. Nothing is leaked — the claim's own
        // release destroys the same resources — but the display engine
        // holds them until then, and that is worth saying.
        tracing::warn!(
            target: "helios_kernel::display",
            %error,
            "a dropped surface stays with the display engine until the claim ends"
        );
    }
}

/// Write `image` into the cursor plane's frame buffer.
///
/// The buffer is shown in no instance's memory, so it is written through
/// the kernel's own alias of its run.
///
/// # Panics
///
/// Panics when `image` is longer than the buffer: the caller checked it
/// against the cursor plane's mode, which is what the buffer was sized
/// for.
fn write_cursor(buffer: &Buffer, image: &[u8]) {
    assert!(
        image.len() as u64 <= buffer.bytes(),
        "a cursor image of {} bytes does not fit a {}-byte frame buffer",
        image.len(),
        buffer.bytes()
    );
    let destination = buffer.kernel_alias().raw() as *mut u8;
    // SAFETY: the run is `buffer.bytes()` long, mapped readable and
    // writable at its kernel alias for as long as `buffer` is held, and
    // shown in no instance's memory. The display engine reads it only
    // while serving a `SetCursor`, which this store's task sends after
    // the write and never concurrently with it.
    unsafe { core::ptr::copy_nonoverlapping(image.as_ptr(), destination, image.len()) };
}

impl<CpuImpl, Net, StorageImpl> display_wit::Host for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn claim(&mut self) -> wasmtime::Result<Result<Resource<DisplayHandle>, display_wit::Error>> {
        let Some(service) = self.runtime_state.display_service() else {
            return Ok(Err(display_wit::Error::Unavailable));
        };
        // Without the display class the answer is the one a machine with
        // no display gives, so a refusal reveals nothing about hardware.
        let Ok(cap) = self.process_authority().derive_display_claim_cap() else {
            return Ok(Err(display_wit::Error::Unavailable));
        };
        if let Err(error) = self.device.claim_display(&service, cap) {
            tracing::warn!(
                target: "helios_kernel::display",
                ?error,
                "an instance was refused the display"
            );
            return Ok(Err(to_wit_error(error)));
        }
        let generation = self
            .device
            .display()
            .claim_ref()
            .expect("a successful claim leaves one")
            .generation();
        let handle = self.table.push(DisplayHandle::new(generation))?;
        tracing::info!(
            target: "helios_kernel::display",
            generation,
            "an instance took ownership of the display"
        );
        Ok(Ok(handle))
    }
}

impl<CpuImpl, Net, StorageImpl> display_wit::HostDisplay for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn drop(&mut self, handle: Resource<DisplayHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(handle)?;
        // Dropping the handle is a compositor saying it is done, and it
        // costs exactly what dying costs: every surface released, every
        // output blanked and every page handed back before the display
        // is offered to anyone else.
        if self
            .device
            .display()
            .claim_ref()
            .is_ok_and(|claim| claim.generation() == handle.generation())
        {
            self.device.display_mut().release();
        }
        Ok(())
    }
}

impl<CpuImpl, Net, StorageImpl> display_wit::HostSurface for StoreData<CpuImpl, Net, StorageImpl>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    fn resolution(
        &mut self,
        handle: Resource<SurfaceHandle>,
    ) -> wasmtime::Result<display_wit::Mode> {
        Ok(to_wit_mode(self.table.get(&handle)?.mode))
    }

    fn drop(&mut self, handle: Resource<SurfaceHandle>) -> wasmtime::Result<()> {
        let surface = self.table.delete(handle)?;
        // The span is the compositor's ordinary memory again from here,
        // whether or not it still holds the display.
        if let Some(view) = surface.view {
            self.views.end(view).map_err(view_trap)?;
        }
        // The resources go back through the owner task, which blanks the
        // output before it drops them; the run stays pinned until it
        // has, because the owner task's record holds it. A surface
        // dropped by an instance that no longer holds the display has
        // already had its resources destroyed by the release.
        let Ok(claim) = self.device.display().claim_ref() else {
            return Ok(());
        };
        if claim.generation() != surface.generation {
            return Ok(());
        }
        let sender = claim.sender();
        let generation = surface.generation;
        let requests = [
            Some(surface.source),
            surface
                .cursor
                .as_ref()
                .map(|(id, _)| SurfaceSource::Framebuffer(*id)),
        ];
        spawn_display_cleanup(self.spawner(), sender, generation, requests);
        Ok(())
    }
}

/// The claim generation a display handle names, and the queue into the
/// owner task.
///
/// Every call that reaches the display engine starts here, and the
/// store is given back before anything is awaited: an `Access` guard
/// held across a wait would hold the store for as long as the display
/// engine takes.
fn claim_of<U, CpuImpl, Net, StorageImpl>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>,
    handle: &Resource<DisplayHandle>,
) -> wasmtime::Result<Result<(u64, DisplaySender), display_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let data = access.get();
    let generation = data.table.get(handle)?.generation();
    Ok(data
        .display_sender(generation)
        .map(|sender| (generation, sender))
        .map_err(to_wit_error))
}

/// What a surface call needs from the store before it waits.
fn surface_claim<U, CpuImpl, Net, StorageImpl>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, StorageImpl>>>,
    handle: &Resource<SurfaceHandle>,
) -> wasmtime::Result<Result<(u64, ScanoutId, DisplaySender), display_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    let data = access.get();
    let surface = data.table.get(handle)?;
    let (generation, scanout) = (surface.generation, surface.scanout);
    Ok(data
        .display_sender(generation)
        .map(|sender| (generation, scanout, sender))
        .map_err(to_wit_error))
}

/// The stream a compositor reads one surface's frames from.
struct VsyncStreamProducer {
    signal: Arc<SequenceSignal>,
    waiter: Option<crate::exec::NotifyWaiter>,
    last_seen: u64,
}

impl Unpin for VsyncStreamProducer {}

impl<T: 'static> StreamProducer<T> for VsyncStreamProducer {
    type Item = display_wit::FrameToken;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        let producer = &mut *self;
        let signal = producer.signal.clone();
        let waiter = producer.waiter.get_or_insert_with(|| signal.waiter());
        match signal.poll_next(cx, waiter, &mut producer.last_seen) {
            Poll::Ready((sequence, nanos)) => {
                destination.set_buffer(Some(display_wit::FrameToken {
                    sequence,
                    presented_nanos: nanos,
                }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// The stream a compositor reads topology changes from.
struct DisplayChangeStreamProducer {
    signal: Arc<SequenceSignal>,
    waiter: Option<crate::exec::NotifyWaiter>,
    last_seen: u64,
}

impl Unpin for DisplayChangeStreamProducer {}

impl<T: 'static> StreamProducer<T> for DisplayChangeStreamProducer {
    type Item = display_wit::DisplayChange;
    type Buffer = Option<Self::Item>;

    fn poll_produce<'a>(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _store: StoreContextMut<'_, T>,
        mut destination: Destination<'a, Self::Item, Self::Buffer>,
        finish: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        if finish {
            return Poll::Ready(Ok(StreamResult::Cancelled));
        }
        let producer = &mut *self;
        let signal = producer.signal.clone();
        let waiter = producer.waiter.get_or_insert_with(|| signal.waiter());
        match signal.poll_next(cx, waiter, &mut producer.last_seen) {
            Poll::Ready((generation, _)) => {
                destination.set_buffer(Some(display_wit::DisplayChange { generation }));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<CpuImpl, Net, StorageImpl, U> display_wit::HostDisplayWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    async fn scanouts(
        accessor: &Accessor<U, Self>,
        handle: Resource<DisplayHandle>,
    ) -> wasmtime::Result<Result<Vec<display_wit::Scanout>, display_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(ControlRequest::Scanouts { generation, reply }, answer)
            .await;
        Ok(outcome
            .map(|scanouts| {
                scanouts
                    .iter()
                    .map(|info| display_wit::Scanout {
                        id: info.id.index(),
                        geometry: to_wit_rect(info.geometry),
                        enabled: info.enabled,
                    })
                    .collect()
            })
            .map_err(to_wit_error))
    }

    async fn modes(
        accessor: &Accessor<U, Self>,
        handle: Resource<DisplayHandle>,
        scanout: u32,
    ) -> wasmtime::Result<Result<Vec<display_wit::Mode>, display_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(
                ControlRequest::Modes {
                    generation,
                    scanout: ScanoutId::new(scanout),
                    reply,
                },
                answer,
            )
            .await;
        // One entry: a 2D display engine publishes one preferred timing
        // per output and scales anything else onto it, so a longer list
        // would be modes the kernel invented rather than modes the
        // display named.
        Ok(outcome
            .map(|mode| alloc::vec![to_wit_mode(mode)])
            .map_err(to_wit_error))
    }

    async fn create(
        accessor: &Accessor<U, Self>,
        handle: Resource<DisplayHandle>,
        scanout: u32,
        mode: display_wit::Mode,
        format: display_wit::PixelFormat,
        buffer: display_wit::Span,
    ) -> wasmtime::Result<Result<Resource<SurfaceHandle>, display_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let scanout = ScanoutId::new(scanout);
        let mode = from_wit_mode(mode);
        let format = from_wit_format(format);
        // A mode with no pixels is a mode no display engine drives, not
        // a span problem: its frame has no size for any span to match.
        let Some(bytes) = mode.frame_bytes(format).filter(|bytes| *bytes != 0) else {
            return Ok(Err(display_wit::Error::UnsupportedMode));
        };
        let span = Span::new(buffer.offset, buffer.length);
        // Pinned and checked first; shown only in the last step, once
        // the display engine has a resource for it. Until then nothing
        // of the frame buffer is in the compositor's memory, so a
        // compositor that stops waiting for this call can free the span
        // at once.
        let (backing, spawner) =
            match accessor.with(|access| prepare_frame(access, span, bytes as u64)) {
                Ok(prepared) => prepared,
                Err(error) => return Ok(Err(to_wit_error(error))),
            };
        let (reply, answer) = oneshot::channel();
        // What the engine makes for a call that is dropped before its
        // last step goes back through the same path a dropped surface's
        // resource does.
        let cleanup = sender.clone();
        let mut pending = PendingReply::new(answer, move |framebuffer| {
            spawn_display_cleanup(
                &spawner,
                cleanup,
                generation,
                [Some(SurfaceSource::Framebuffer(framebuffer)), None],
            );
        });
        let queued = sender
            .control_oneway(ControlRequest::CreateSurface {
                generation,
                scanout,
                mode,
                format,
                backing: backing.clone(),
                reply,
            })
            .await;
        let framebuffer = match queued {
            Ok(()) => pending.reply(DisplayServiceError::Closed).await,
            Err(error) => Err(error),
        };
        let framebuffer = match framebuffer {
            Ok(framebuffer) => framebuffer,
            // Nothing will ever latch this frame buffer, and nothing
            // showed it, so the run goes back with this call's reference.
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let shown = accessor.with(|access| {
            show_frame(access, span, &backing, |view| SurfaceHandle {
                generation,
                scanout,
                mode,
                format,
                source: SurfaceSource::Framebuffer(framebuffer),
                view: Some(view),
                cursor: None,
                vsync: Arc::new(SequenceSignal::new()),
            })
        })?;
        match shown {
            Ok(surface) => {
                pending.keep();
                Ok(Ok(surface))
            }
            Err(error) => {
                // The span was taken while the engine was answering. The
                // resource goes back before the refusal does; the pending
                // reply stays armed across the wait, so a call dropped in
                // it still hands the resource back.
                if let Err(closed) = sender
                    .control_oneway(ControlRequest::DestroySurface {
                        generation,
                        surface: SurfaceSource::Framebuffer(framebuffer),
                    })
                    .await
                {
                    tracing::warn!(
                        target: "helios_kernel::display",
                        %closed,
                        "a surface whose span was taken could not be handed back to the display owner"
                    );
                }
                pending.keep();
                Ok(Err(to_wit_error(error)))
            }
        }
    }

    async fn create_from_blob(
        accessor: &Accessor<U, Self>,
        handle: Resource<DisplayHandle>,
        scanout: u32,
        mode: display_wit::Mode,
        format: display_wit::PixelFormat,
        blob: Resource<BlobHandle>,
        offset: u32,
        stride: u32,
    ) -> wasmtime::Result<Result<Resource<SurfaceHandle>, display_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let scanout = ScanoutId::new(scanout);
        let mode = from_wit_mode(mode);
        let format = from_wit_format(format);
        // The blob has to be one this instance's open gpu session holds:
        // a handle that outlived its session names a resource the
        // session's end already destroyed. The scanout's hold is taken
        // in the same step, so a release that starts after it — the
        // handle dropped, the session ended — leaves the blob alone
        // until the scanout has let go.
        let named = accessor.with(
            |mut access| -> wasmtime::Result<Result<(BlobId, ScanoutHold), DisplayServiceError>> {
                let data = access.get();
                let blob = data.table.get(&blob)?;
                if !data.device.gpu().is_current(blob.session) {
                    return Ok(Err(DisplayServiceError::StaleBlob));
                }
                Ok(Ok((blob.id, ScanoutHold::new(&blob.shared))))
            },
        )?;
        let (blob, hold) = match named {
            Ok(named) => named,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let layout = BlobScanout {
            blob,
            mode,
            format,
            offset,
            stride,
        };
        let (reply, answer) = oneshot::channel();
        let shown = sender
            .control(
                ControlRequest::CreateBlobSurface {
                    generation,
                    scanout,
                    layout,
                    hold,
                    reply,
                },
                answer,
            )
            .await;
        if let Err(error) = shown {
            return Ok(Err(to_wit_error(error)));
        }
        let surface = accessor.with(|mut access| {
            access.get().table.push(SurfaceHandle {
                generation,
                scanout,
                mode,
                format,
                source: SurfaceSource::Blob { blob, scanout },
                view: None,
                cursor: None,
                vsync: Arc::new(SequenceSignal::new()),
            })
        })?;
        Ok(Ok(surface))
    }

    fn changed(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<DisplayHandle>,
    ) -> wasmtime::Result<StreamReader<display_wit::DisplayChange>> {
        let generation = accessor.get().table.get(&handle)?.generation();
        let signal = accessor
            .get()
            .device
            .display()
            .claim_ref()
            .map_err(|error| wasmtime::Error::msg(alloc::format!("{error}")))?
            .changes();
        // From now on, not from the machine's first announcement: a
        // compositor that has just claimed the display reads the current
        // topology with `scanouts`, and would otherwise be handed every
        // change since boot as news.
        let last_seen = signal.sequence();
        debug_assert_ne!(generation, 0, "a claim generation starts at one");
        StreamReader::new(
            &mut accessor,
            DisplayChangeStreamProducer {
                signal,
                waiter: None,
                last_seen,
            },
        )
    }
}

impl<CpuImpl, Net, StorageImpl, U> display_wit::HostSurfaceWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, StorageImpl>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    StorageImpl: crate::Storage,
{
    async fn set_cursor(
        accessor: &Accessor<U, Self>,
        handle: Resource<SurfaceHandle>,
        image: Vec<u8>,
        hotspot: display_wit::Point,
    ) -> wasmtime::Result<Result<(), display_wit::Error>> {
        let (generation, scanout, format, held) = accessor.with(|mut access| {
            let surface = access.get().table.get(&handle)?;
            Ok::<_, wasmtime::Error>((
                surface.generation,
                surface.scanout,
                surface.format,
                surface.cursor.clone(),
            ))
        })?;
        let expected = CursorImage::MODE
            .frame_bytes(format)
            .expect("a 64 by 64 cursor plane fits any address space");
        if image.len() != expected {
            return Ok(Err(display_wit::Error::UnsupportedMode));
        }
        let sender = match accessor.with(|mut access| access.get().display_sender(generation)) {
            Ok(sender) => sender,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        // The plane's resource is built once and rewritten afterwards: a
        // compositor that animates its pointer would otherwise pin a
        // frame per image.
        let (framebuffer, frame) = match held {
            Some(cursor) => cursor,
            None => {
                // Pinned inside this host call, so the run is charged to
                // this instance like any frame buffer it shows.
                let frame = match Buffer::pinned(expected as u64) {
                    Ok(frame) => frame,
                    Err(error) => return Ok(Err(to_wit_error(error.into()))),
                };
                let (reply, answer) = oneshot::channel();
                let created = sender
                    .control(
                        ControlRequest::CreateCursor {
                            generation,
                            format,
                            backing: frame.clone(),
                            reply,
                        },
                        answer,
                    )
                    .await;
                match created {
                    Ok(framebuffer) => {
                        let held = frame.clone();
                        accessor.with(|mut access| {
                            access.get().table.get_mut(&handle).map(|surface| {
                                surface.cursor = Some((framebuffer, held));
                            })
                        })?;
                        (framebuffer, frame)
                    }
                    // The owner task dropped its reference with the
                    // refusal, so the run goes back with this one.
                    Err(error) => return Ok(Err(to_wit_error(error))),
                }
            }
        };
        write_cursor(&frame, &image);
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .control(
                ControlRequest::SetCursor {
                    generation,
                    scanout,
                    position: Point::new(0, 0),
                    image: CursorImage {
                        framebuffer,
                        hotspot: from_wit_point(hotspot),
                    },
                    reply,
                },
                answer,
            )
            .await;
        Ok(outcome.map_err(to_wit_error))
    }

    async fn move_cursor(
        accessor: &Accessor<U, Self>,
        handle: Resource<SurfaceHandle>,
        position: display_wit::Point,
    ) -> wasmtime::Result<Result<(), display_wit::Error>> {
        let (generation, scanout, sender) =
            match accessor.with(|access| surface_claim(access, &handle))? {
                Ok(triple) => triple,
                Err(error) => return Ok(Err(error)),
            };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .cursor(
                CursorRequest::Move {
                    generation,
                    scanout,
                    position: from_wit_point(position),
                    reply,
                },
                answer,
            )
            .await;
        Ok(outcome.map_err(to_wit_error))
    }

    async fn hide_cursor(
        accessor: &Accessor<U, Self>,
        handle: Resource<SurfaceHandle>,
    ) -> wasmtime::Result<Result<(), display_wit::Error>> {
        let (generation, scanout, sender) =
            match accessor.with(|access| surface_claim(access, &handle))? {
                Ok(triple) => triple,
                Err(error) => return Ok(Err(error)),
            };
        let (reply, answer) = oneshot::channel();
        let outcome = sender
            .cursor(
                CursorRequest::Hide {
                    generation,
                    scanout,
                    reply,
                },
                answer,
            )
            .await;
        Ok(outcome.map_err(to_wit_error))
    }

    fn vsync(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<SurfaceHandle>,
    ) -> wasmtime::Result<StreamReader<display_wit::FrameToken>> {
        let surface = accessor.get().table.get(&handle)?;
        let signal = surface.vsync.clone();
        let last_seen = signal.sequence();
        StreamReader::new(
            &mut accessor,
            VsyncStreamProducer {
                signal,
                waiter: None,
                last_seen,
            },
        )
    }

    fn present(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<SurfaceHandle>,
        region: display_wit::Rect,
    ) -> wasmtime::Result<FutureReader<Result<display_wit::FrameToken, display_wit::Error>>> {
        let data = accessor.get();
        let surface = data.table.get(&handle)?;
        let generation = surface.generation;
        let source = surface.source;
        let mode = surface.mode;
        let vsync = surface.vsync.clone();
        let region = from_wit_rect(region);
        // The rectangle is checked here rather than at the device: one
        // outside the surface names memory the compositor does not own,
        // and the answer is the same whether or not the display engine
        // would have caught it.
        let sender = if region.fits_in(mode) {
            data.display_sender(generation)
        } else {
            Err(DisplayServiceError::OutOfBounds)
        };
        let sender = match sender {
            Ok(sender) => sender,
            Err(error) => {
                let error = to_wit_error(error);
                return FutureReader::new(&mut accessor, async move {
                    Ok::<_, wasmtime::Error>(Err(error))
                });
            }
        };
        FutureReader::new(&mut accessor, async move {
            let (reply, answer) = oneshot::channel();
            let outcome = sender
                .control(
                    ControlRequest::Present {
                        generation,
                        surface: source,
                        region,
                        vsync,
                        reply,
                    },
                    answer,
                )
                .await;
            Ok::<_, wasmtime::Error>(
                outcome
                    .map(|token| display_wit::FrameToken {
                        sequence: token.sequence,
                        presented_nanos: token.presented_nanos,
                    })
                    .map_err(to_wit_error),
            )
        })
    }
}
