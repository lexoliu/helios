//! `helios:system/surface` for the component host.
//!
//! This is the whole of what a program sees of the desktop, and the
//! kernel implements none of it. A `create` pins the client's frame
//! buffer, mints an identity, hands the identity and the buffer to the
//! compositor through the provider slot and, once the compositor has
//! answered, shows the buffer at the span the client named; a `commit`
//! is one message; the events a client reads are the ones the
//! compositor routed to it with `deliver`. The kernel validates,
//! forwards and accounts, and never touches a pixel.
//!
//! A `create` the guest stops waiting for shows nothing: the view is
//! taken in the call's last step, after its last wait, and the window
//! the compositor was told about goes back to the compositor's
//! supervisor from the pending window's drop.
//!
//! The surfaces live in the instance's store, so they are single-owned:
//! the resource handle a program holds carries the registry entry and
//! the view its pixels are shown through, and nothing else. A handle
//! that outlived its surface — the client dropped it, or the compositor
//! died under it — names a window that is no longer on the desktop and
//! is refused, rather than being answered against whatever is there now.
//!
//! # Concurrency contract
//!
//! Every call here runs on the task that owns the store, so the view
//! table and the held list need no lock, and the client's span is
//! substituted by that task alone. What is shared is the registry,
//! which carries its own, the provider queue into the compositor, which
//! carries its own, the frame buffer, whose count is atomic, and each
//! surface's event queue, whose reader arms its wait before it looks.

use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::channel::oneshot;
use triomphe::Arc;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    Access, Accessor, Destination, HasSelf, Linker, Resource, StreamProducer, StreamReader,
    StreamResult, VecBuffer,
};

use crate::ComponentHostNetwork;
use crate::buffer::{Span, ViewId};
use crate::component::ProviderError;
use crate::surface::{
    SurfaceCreate, SurfaceEvents, SurfaceGeometry, SurfaceId, SurfaceRect, SurfaceRequest,
    SurfaceServiceError, SurfaceShared,
};
use crate::wasmtime_adapter::bindings::surface::bindings::helios::system::surface as surface_wit;

use super::super::{StoreData, guest_memory, view_trap};

/// Register `helios:system/surface` in a linker.
///
/// Every program linker gets it, for the reason the display interface
/// does: the window is the capability, not the import. A program that
/// never asks for one learns nothing from having it, and one that asks
/// on a machine with no compositor is told `unavailable`.
pub(in crate::wasmtime_adapter::component_host) fn add_surface_to_linker<CpuImpl, Net, HostFs>(
    linker: &mut Linker<StoreData<CpuImpl, Net, HostFs>>,
) -> wasmtime::Result<()>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    surface_wit::add_to_linker::<_, HasSelf<StoreData<CpuImpl, Net, HostFs>>>(linker, |state| state)
}

/// One client window, as its store records it.
pub struct ClientSurfaceHandle {
    shared: Arc<SurfaceShared>,
    /// The frame buffer's view at the client's span, ended when the
    /// handle is dropped.
    view: ViewId,
}

const fn to_wit_error(error: SurfaceServiceError) -> surface_wit::Error {
    match error {
        SurfaceServiceError::Unavailable => surface_wit::Error::Unavailable,
        SurfaceServiceError::NoCompositor => surface_wit::Error::NoCompositor,
        SurfaceServiceError::UnsupportedSize => surface_wit::Error::UnsupportedSize,
        SurfaceServiceError::TooManySurfaces => surface_wit::Error::TooManySurfaces,
        SurfaceServiceError::OutOfBounds => surface_wit::Error::OutOfBounds,
        SurfaceServiceError::BadSpan(_) | SurfaceServiceError::CompositorBadSpan => {
            surface_wit::Error::BadSpan
        }
        SurfaceServiceError::OutOfMemory => surface_wit::Error::OutOfMemory,
        SurfaceServiceError::Gone => surface_wit::Error::Gone,
        SurfaceServiceError::NotTheCompositor => surface_wit::Error::NotTheCompositor,
    }
}

const fn from_wit_rect(rect: surface_wit::Rect) -> SurfaceRect {
    SurfaceRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

/// How a provider queue that will not take a message reads to a client.
const fn provider_error(error: ProviderError) -> SurfaceServiceError {
    match error {
        // No compositor was provisioned in this kernel image at all.
        ProviderError::Unavailable => SurfaceServiceError::Unavailable,
        // One was, and it is not taking work now. A queue that is full
        // reaches here only from a caller that could not wait for room,
        // and what that caller lost is the same as a compositor that has
        // stopped answering.
        ProviderError::Closed | ProviderError::Full => SurfaceServiceError::NoCompositor,
    }
}

impl<CpuImpl, Net, HostFs> surface_wit::Host for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
}

impl<CpuImpl, Net, HostFs> surface_wit::HostSurface for StoreData<CpuImpl, Net, HostFs>
where
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn size(
        &mut self,
        handle: Resource<ClientSurfaceHandle>,
    ) -> wasmtime::Result<surface_wit::Size> {
        let handle = self.table.get(&handle)?;
        let geometry = handle.shared.geometry();
        Ok(surface_wit::Size {
            width: geometry.width,
            height: geometry.height,
        })
    }

    fn drop(&mut self, handle: Resource<ClientSurfaceHandle>) -> wasmtime::Result<()> {
        let handle = self.table.delete(handle)?;
        let id = handle.shared.id();
        let registry = self.runtime_state.surface_service();
        // The client's span is its ordinary memory again once the drop
        // returns. The window comes off the desktop here too, and goes
        // to the compositor's supervisor on the unbounded return queue —
        // the provider queue is bounded, and a destroy it had no room
        // for would leave the compositor's view, and the run behind it,
        // in place for good. The supervisor ends that view and tells the
        // compositor; the frame buffer stays pinned until then, because
        // the compositor may be composing from it now.
        self.views.end(handle.view).map_err(view_trap)?;
        self.device.surfaces_mut().release(&registry, id);
        Ok(())
    }
}

/// The stream a program reads the input its window was given from.
struct SurfaceEventStreamProducer {
    events: SurfaceEvents,
}

impl Unpin for SurfaceEventStreamProducer {}

impl<T: 'static> StreamProducer<T> for SurfaceEventStreamProducer {
    type Item = surface_wit::InputEvent;
    type Buffer = VecBuffer<Self::Item>;

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
        match self.events.poll_burst(cx) {
            Poll::Ready(Some(burst)) => {
                let events: Vec<Self::Item> = burst
                    .into_iter()
                    .map(|event| surface_wit::InputEvent {
                        kind: event.kind,
                        code: event.code,
                        value: event.value,
                    })
                    .collect();
                destination.set_buffer(VecBuffer::from(events));
                Poll::Ready(Ok(StreamResult::Completed))
            }
            // The window is off the desktop. A reader that ended is how
            // a client learns that without polling for it.
            Poll::Ready(None) => Poll::Ready(Ok(StreamResult::Dropped)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<CpuImpl, Net, HostFs, U> surface_wit::HostWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    async fn create(
        accessor: &Accessor<U, Self>,
        width: u32,
        height: u32,
        buffer: surface_wit::Span,
    ) -> wasmtime::Result<Result<Resource<ClientSurfaceHandle>, surface_wit::Error>> {
        let geometry = SurfaceGeometry { width, height };
        let span = Span::new(buffer.offset, buffer.length);
        // The frame buffer and the identity come first, on the store's
        // own task: the compositor is handed the buffer, so it cannot be
        // told before there is one. Pinned inside this host call, so the
        // run is charged to this client. Nothing is shown yet: until the
        // call's last step the window is a pending one, which a guest
        // that drops this call's future hands back to the compositor's
        // supervisor as it goes.
        let prepared = accessor.with(|mut access| {
            let linear_memory = access.get().linear_memory;
            let memory = guest_memory(linear_memory, &access);
            let data = access.get();
            let registry = data.runtime_state.surface_service();
            let runtime_state = data.runtime_state.clone();
            let pending = memory
                .map_err(SurfaceServiceError::from)
                .and_then(|memory| {
                    data.device.surfaces_mut().prepare(
                        &registry,
                        &data.views,
                        memory,
                        span,
                        geometry,
                    )
                });
            (pending, runtime_state)
        });
        let (pending, runtime_state) = prepared;
        let pending = match pending {
            Ok(pending) => pending,
            Err(error) => return Ok(Err(to_wit_error(error))),
        };

        let id = pending.id();
        let (reply, answer) = oneshot::channel();
        let queued = runtime_state
            .compositor()
            .send(SurfaceRequest::Create(SurfaceCreate {
                id,
                geometry,
                buffer: pending.buffer().clone(),
                reply,
            }))
            .await;
        let accepted = match queued {
            Ok(()) => match answer.await {
                Ok(accepted) => accepted,
                // The compositor died with this create in flight.
                Err(oneshot::Canceled) => Err(SurfaceServiceError::NoCompositor),
            },
            Err(error) => Err(provider_error(error)),
        };

        // The last step, and the only one that shows anything: the view
        // at the client's span and the handle that owns it are recorded
        // together, with no wait in between that a cancellation could
        // land in.
        accessor.with(|mut access| {
            let linear_memory = access.get().linear_memory;
            // The store's memory handle is recorded once, when the
            // instance is built, and `prepare` already read it.
            let memory = guest_memory(linear_memory, &access)
                .expect("a store whose memory took the span when the window was prepared has one");
            let data = access.get();
            let registry = data.runtime_state.surface_service();
            let surfaces = data.device.surfaces_mut();
            if let Err(error) = accepted {
                // The compositor holds nothing of the window, so it is
                // only taken off the desktop, and the run goes back with
                // the pending window's reference.
                surfaces.refused(&registry, pending);
                return Ok(Err(to_wit_error(error)));
            }
            // A span something else was shown over while the compositor
            // was answering is refused here, and the pending window goes
            // back to the supervisor, which tells the compositor.
            let (shared, view) = match surfaces.show(&mut data.views, memory, pending) {
                Ok(shown) => shown,
                Err(error) => return Ok(Err(to_wit_error(error))),
            };
            let handle = data.table.push(ClientSurfaceHandle { shared, view })?;
            tracing::info!(
                target: "helios_kernel::surface",
                surface = id.raw(),
                width,
                height,
                "a program took a window on the desktop"
            );
            Ok(Ok(handle))
        })
    }

    async fn deliver(
        accessor: &Accessor<U, Self>,
        id: u64,
        events: Vec<surface_wit::InputEvent>,
    ) -> wasmtime::Result<Result<(), surface_wit::Error>> {
        let id = SurfaceId::from_raw(id);
        let (registry, caller) = accessor.with(|mut access| {
            let data = access.get();
            (data.runtime_state.surface_service(), data.instance().id())
        });
        if !registry.is_compositor(caller) {
            return Ok(Err(surface_wit::Error::NotTheCompositor));
        }
        let Some(surface) = registry.lookup(id) else {
            return Ok(Err(surface_wit::Error::Gone));
        };
        let report: Vec<helios_hal::input::InputEvent> = events
            .into_iter()
            .map(|event| helios_hal::input::InputEvent {
                kind: event.kind,
                code: event.code,
                value: event.value,
            })
            .collect();
        // A report that did not fit is a client that is not reading its
        // own window, which is the client's problem rather than the
        // compositor's: the compositor is told it landed either way, so
        // it never waits on a reader it does not own.
        surface.publish_report(&report);
        Ok(Ok(()))
    }
}

impl<CpuImpl, Net, HostFs, U> surface_wit::HostSurfaceWithStore<U>
    for HasSelf<StoreData<CpuImpl, Net, HostFs>>
where
    U: 'static,
    CpuImpl: helios_hal::cpu::Cpu + Clone,
    Net: ComponentHostNetwork,
    HostFs: crate::HostFileSystem,
{
    fn events(
        mut accessor: Access<'_, U, Self>,
        handle: Resource<ClientSurfaceHandle>,
    ) -> wasmtime::Result<StreamReader<surface_wit::InputEvent>> {
        // The store is given back between each step: a borrow of it held
        // across `StreamReader::new` would hold the store for as long as
        // the reader exists.
        //
        // Armed here, before the reader is handed over: an event routed
        // between this call and the program's first read is one the
        // program is owed.
        let events = {
            let named = accessor.get().table.get(&handle)?;
            SurfaceShared::events(&named.shared)
        };
        StreamReader::new(&mut accessor, SurfaceEventStreamProducer { events })
    }

    async fn commit(
        accessor: &Accessor<U, Self>,
        handle: Resource<ClientSurfaceHandle>,
        region: surface_wit::Rect,
    ) -> wasmtime::Result<Result<(), surface_wit::Error>> {
        let region = from_wit_rect(region);
        let named = accessor.with(|mut access| {
            let data = access.get();
            let handle = data.table.get(&handle)?;
            let (id, geometry, alive) = (
                handle.shared.id(),
                handle.shared.geometry(),
                handle.shared.is_alive(),
            );
            Ok::<_, wasmtime::Error>((id, geometry, alive, data.runtime_state.clone()))
        })?;
        let (id, geometry, alive, runtime_state) = named;
        if !alive {
            return Ok(Err(surface_wit::Error::Gone));
        }
        if !region.fits(geometry) {
            return Ok(Err(surface_wit::Error::OutOfBounds));
        }

        let (reply, answer) = oneshot::channel();
        let queued = runtime_state
            .compositor()
            .send(SurfaceRequest::Commit { id, region, reply })
            .await;
        Ok(match queued {
            Ok(()) => match answer.await {
                Ok(result) => result.map_err(to_wit_error),
                Err(oneshot::Canceled) => Err(surface_wit::Error::NoCompositor),
            },
            Err(error) => Err(to_wit_error(provider_error(error))),
        })
    }
}
