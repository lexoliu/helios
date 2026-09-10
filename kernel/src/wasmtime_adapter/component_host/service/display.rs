//! `helios:system/display` for the component host.
//!
//! This is the whole of what a compositor sees of the machine's display,
//! and none of it is on the path a frame takes to the screen. A surface
//! is created once; from then on the compositor writes pixels into its
//! own linear memory with ordinary stores, and `present` is one message
//! to the task that owns the display engine, which turns it into one
//! copy into the device's own copy of the resource and one flush.
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
//! and its arena need no lock. What is shared is the queue into the
//! owner tasks, which carries its own synchronisation, and the two
//! signals a stream is driven by — one per surface for frames, one per
//! device for topology — which are counted events whose readers arm the
//! notification before they read the count.

use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use helios_hal::display::{
    CursorImage, DisplayMode, FramebufferId, PixelFormat, Point, Rect, ScanoutId,
};
use triomphe::Arc;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, FutureReader, HasSelf, Linker, Resource, StreamProducer,
    StreamReader, StreamResult,
};

use crate::ComponentHostNetwork;
use crate::display::PinnedFrame;
use crate::display::{
    ControlRequest, CursorRequest, DisplaySender, DisplayServiceError, SequenceSignal,
};
use crate::wasmtime_adapter::bindings::display::bindings::helios::system::display as display_wit;

use super::super::StoreData;

/// Register `helios:system/display` in a linker.
///
/// Every program linker gets it, for the reason the device interface
/// does: the claim is the capability, not the import. A program that
/// never claims the display learns nothing from having the import, and
/// one that claims it on a machine with no display device is told
/// `unavailable`.
pub(in crate::wasmtime_adapter::component_host) fn add_display_to_linker<CpuImpl, Net, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, Net, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    display_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(linker, |state| state)
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
    framebuffer: FramebufferId,
    frame: PinnedFrame,
    /// The cursor plane's own resource and pages, once this surface has
    /// been given a pointer image. A compositor that never sets one
    /// pays nothing for the plane existing.
    cursor: Option<(FramebufferId, PinnedFrame)>,
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
        DisplayServiceError::WindowExhausted => display_wit::Error::WindowExhausted,
        DisplayServiceError::OutOfMemory => display_wit::Error::OutOfMemory,
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

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    /// The queue into the display owner, checked against `generation`.
    fn display_sender(&self, generation: u64) -> Result<DisplaySender, DisplayServiceError> {
        let claim = self.device.display().claim_ref()?;
        if claim.generation() != generation {
            return Err(DisplayServiceError::NotClaimed);
        }
        Ok(claim.sender())
    }

    /// Write `image` into the pages `frame` covers.
    ///
    /// The pages are in this instance's linear memory, above everything
    /// it has grown into, so they are reached through the base the
    /// runtime recorded rather than through the memory's accessible
    /// slice — which stops at the instance's current size and would
    /// never contain the display window.
    ///
    /// # Panics
    ///
    /// Panics when the instance's linear memory has not been recorded.
    /// A claim is refused before that happens, so a surface cannot
    /// exist without it.
    fn write_frame(&self, frame: PinnedFrame, image: &[u8]) {
        let memory = self
            .device
            .memory()
            .expect("an instance holding the display has a recorded linear memory");
        let base = memory.base.raw() as *mut u8;
        assert!(
            image.len() as u64 <= frame.bytes,
            "a cursor image of {} bytes does not fit a {}-byte frame buffer",
            image.len(),
            frame.bytes
        );
        // SAFETY: `frame` was committed by this instance's own arena at
        // `frame.offset` inside the linear memory based at `base`, is
        // `frame.bytes` long, and is mapped readable and writable until
        // the arena releases it. The store owns both the arena and this
        // call, so nothing else is writing it.
        let destination = unsafe {
            core::slice::from_raw_parts_mut(
                base.add(usize::try_from(frame.offset).expect("a window offset fits a pointer")),
                image.len(),
            )
        };
        destination.copy_from_slice(image);
    }
}

impl<CpuImpl, Net, HostFs> display_wit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn claim(&mut self) -> wasmtime::Result<Result<Resource<DisplayHandle>, display_wit::Error>> {
        let Some(service) = self.runtime_state.display_service() else {
            return Ok(Err(display_wit::Error::Unavailable));
        };
        if let Err(error) = self.device.claim_display(&service) {
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

impl<CpuImpl, Net, HostFs> display_wit::HostDisplay for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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

impl<CpuImpl, Net, HostFs> display_wit::HostSurface for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn buffer(
        &mut self,
        handle: Resource<SurfaceHandle>,
    ) -> wasmtime::Result<display_wit::Placement> {
        let surface = self.table.get(&handle)?;
        Ok(display_wit::Placement {
            offset: surface.frame.offset,
            length: surface.frame.bytes,
        })
    }

    fn resolution(
        &mut self,
        handle: Resource<SurfaceHandle>,
    ) -> wasmtime::Result<display_wit::Mode> {
        Ok(to_wit_mode(self.table.get(&handle)?.mode))
    }

    fn drop(&mut self, handle: Resource<SurfaceHandle>) -> wasmtime::Result<()> {
        let surface = self.table.delete(handle)?;
        // The resources go back through the owner task, which blanks the
        // output before it drops them; the pages stay pinned until it
        // has, and are released with the claim. A surface dropped by an
        // instance that no longer holds the display has already had both
        // done for it.
        let Ok(claim) = self.device.display().claim_ref() else {
            return Ok(());
        };
        if claim.generation() != surface.generation {
            return Ok(());
        }
        let sender = claim.sender();
        let generation = surface.generation;
        let requests = [Some(surface.framebuffer), surface.cursor.map(|(id, _)| id)];
        self.spawn_display_cleanup(sender, generation, requests);
        Ok(())
    }
}

impl<CpuImpl, Net, HostFs> StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    /// Tell the owner task to let go of the resources a dropped surface
    /// held.
    ///
    /// A resource drop is synchronous and the display engine is not, so
    /// the work is handed to a task rather than awaited here. It cannot
    /// be lost: if the instance dies before the task runs, the claim's
    /// own release destroys the same resources.
    fn spawn_display_cleanup(
        &self,
        sender: DisplaySender,
        generation: u64,
        framebuffers: [Option<FramebufferId>; 2],
    ) {
        let spawned = self.spawner().try_spawn_detached(async move {
            for framebuffer in framebuffers.into_iter().flatten() {
                if let Err(error) = sender
                    .control_oneway(ControlRequest::DestroySurface {
                        generation,
                        framebuffer,
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
            // The task arena is full. Nothing is leaked — the claim's
            // own release destroys the same resources — but the display
            // engine holds them until then, and that is worth saying.
            tracing::warn!(
                target: "helios_kernel::display",
                %error,
                "a dropped surface stays with the display engine until the claim ends"
            );
        }
    }
}

/// The claim generation a display handle names, and the queue into the
/// owner task.
///
/// Every call that reaches the display engine starts here, and the
/// store is given back before anything is awaited: an `Access` guard
/// held across a wait would hold the store for as long as the display
/// engine takes.
fn claim_of<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<DisplayHandle>,
) -> wasmtime::Result<Result<(u64, DisplaySender), display_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    let data = access.get();
    let generation = data.table.get(handle)?.generation();
    Ok(data
        .display_sender(generation)
        .map(|sender| (generation, sender))
        .map_err(to_wit_error))
}

/// What a surface call needs from the store before it waits.
fn surface_claim<U, CpuImpl, Net, HostFs>(
    mut access: Access<'_, U, HasSelf<StoreData<CpuImpl, Net, HostFs>>>,
    handle: &Resource<SurfaceHandle>,
) -> wasmtime::Result<Result<(u64, ScanoutId, DisplaySender), display_wit::Error>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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

impl<CpuImpl, Net, HostFs, U> display_wit::HostDisplayWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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
    ) -> wasmtime::Result<Result<Resource<SurfaceHandle>, display_wit::Error>> {
        let (generation, sender) = match accessor.with(|access| claim_of(access, &handle))? {
            Ok(pair) => pair,
            Err(error) => return Ok(Err(error)),
        };
        let scanout = ScanoutId::new(scanout);
        let mode = from_wit_mode(mode);
        let format = from_wit_format(format);
        let frame = accessor.with(|mut access| {
            access
                .get()
                .device
                .display_mut()
                .pin_frame(mode, format.bytes_per_pixel())
        });
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };
        let (reply, answer) = oneshot::channel();
        let created = sender
            .control(
                ControlRequest::CreateSurface {
                    generation,
                    scanout,
                    mode,
                    format,
                    backing: frame.backing,
                    reply,
                },
                answer,
            )
            .await;
        let framebuffer = match created {
            Ok(framebuffer) => framebuffer,
            Err(error) => {
                // Nothing is showing these pages, so they go straight
                // back rather than waiting for a release that will never
                // come.
                accessor.with(|mut access| access.get().device.display_mut().unpin_frame(frame));
                return Ok(Err(to_wit_error(error)));
            }
        };
        let surface = accessor.with(|mut access| {
            access.get().table.push(SurfaceHandle {
                generation,
                scanout,
                mode,
                format,
                framebuffer,
                frame,
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

impl<CpuImpl, Net, HostFs, U> display_wit::HostSurfaceWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
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
                surface.cursor,
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
                let frame = accessor.with(|mut access| {
                    access
                        .get()
                        .device
                        .display_mut()
                        .pin_frame(CursorImage::MODE, format.bytes_per_pixel())
                });
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => return Ok(Err(to_wit_error(error))),
                };
                let (reply, answer) = oneshot::channel();
                let created = sender
                    .control(
                        ControlRequest::CreateCursor {
                            generation,
                            format,
                            backing: frame.backing,
                            reply,
                        },
                        answer,
                    )
                    .await;
                match created {
                    Ok(framebuffer) => {
                        accessor.with(|mut access| {
                            access.get().table.get_mut(&handle).map(|surface| {
                                surface.cursor = Some((framebuffer, frame));
                            })
                        })?;
                        (framebuffer, frame)
                    }
                    Err(error) => {
                        accessor.with(|mut access| {
                            access.get().device.display_mut().unpin_frame(frame);
                        });
                        return Ok(Err(to_wit_error(error)));
                    }
                }
            }
        };
        accessor.with(|mut access| access.get().write_frame(frame, &image));
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
        let framebuffer = surface.framebuffer;
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
                        framebuffer,
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
