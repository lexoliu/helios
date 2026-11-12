//! One instance's side of the surface path.
//!
//! Two kinds of instance carry this and they carry different halves of
//! it. A client holds the windows it created; each one's frame buffer is
//! pinned from the client's own pool and shown at the span the client
//! named. The compositor holds a view of every client's frame buffer, at
//! a span of its own memory it named, and creates no window of its own.
//!
//! The views themselves are in the store's [`ViewTable`]; what is here
//! is which window each of the compositor's views belongs to, and which
//! windows a client has to hand back when it dies.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so nothing
//! here is shared and nothing here locks. The registry and the queues
//! behind [`SurfaceService`] are the shared part and carry their own
//! synchronisation.
//!
//! # A create that does not finish
//!
//! A client's `create` is told about by the compositor before it ends,
//! and the client may stop waiting for it — a guest that drops the
//! import's future cancels the host call, and the SDK then frees the
//! pages the span named. So the client's view is shown only in the
//! call's last step, after the compositor has answered, and until then
//! the window is a [`PendingSurface`]: dropped unfinished, it hands the
//! window to the compositor's supervisor the way a dying client does,
//! through the return queue, which needs no store and cannot fail. A
//! call that never finishes leaves nothing shown in the client's memory.
//!
//! # What a drop has to do, and what it must not
//!
//! Dropping this — which is what killing an instance does — has to end
//! with every window this client held off the desktop, and it cannot
//! await anything. So it does not tell the compositor (that is
//! asynchronous): it hands the windows' identities to the compositor's
//! supervisor and raises the return signal. The client's own views were
//! withdrawn with its view table; the compositor's views of the same
//! buffers are ended by the supervisor, and each buffer goes back to the
//! client's pool with the last of those references.

use arrayvec::ArrayVec;
use triomphe::Arc;

use crate::buffer::{Buffer, GuestMemory, Span, ViewError, ViewId, ViewTable};

use super::SurfaceServiceError;
use super::service::{
    MAX_INSTANCE_SURFACES, MAX_LIVE_SURFACES, ReturnedSurfaces, SurfaceGeometry, SurfaceId,
    SurfaceService, SurfaceShared,
};

/// A window registered and on its way to the compositor, whose client
/// has not finished creating it.
///
/// Nothing of it is shown in the client's memory yet. Dropping it
/// unfinished — the client cancelled its `create` — takes the window off
/// the desktop and hands its identity to the compositor's supervisor,
/// which ends the compositor's view of it, if it took one, and tells the
/// compositor it is gone. That path needs no store, so it is the one a
/// future dropped by its guest can take.
pub struct PendingSurface {
    shared: Arc<SurfaceShared>,
    /// The frame buffer, which the compositor is handed a reference to.
    buffer: Buffer,
    /// The client's span, checked when the window was registered and
    /// checked again when the buffer is shown there.
    span: Span,
    /// The registry the window goes back through. Taken when the create
    /// finishes, either way, so a finished one is not handed back.
    registry: Option<SurfaceService>,
}

impl PendingSurface {
    pub fn id(&self) -> SurfaceId {
        self.shared.id()
    }

    /// The frame buffer, for the compositor's own view of it.
    pub const fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Finish the create: the window is no longer the pending one's to
    /// hand back.
    fn settle(mut self) -> Arc<SurfaceShared> {
        self.registry = None;
        self.shared.clone()
    }
}

impl Drop for PendingSurface {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.return_surfaces(ReturnedSurfaces::one(self.shared.id()));
        }
    }
}

/// One instance's hold on the surface path.
#[derive(Default)]
pub struct SurfaceOwnership {
    /// The windows this instance owns, and the ones it is creating.
    ///
    /// A window handed back — by a create that did not finish, a resource
    /// drop, or this instance's death — stays here, and counted, until
    /// the compositor's supervisor has drained it
    /// ([`SurfaceShared::holds_slot`]); it is never handed back a second
    /// time. An entry that holds no slot any more — drained, or retired
    /// with a compositor that died — is pruned.
    ///
    /// Written only by this instance's processor. The per-window state it
    /// reads is written by the processor that hands the window back and
    /// the one that drains it.
    held: ArrayVec<Arc<SurfaceShared>, MAX_INSTANCE_SURFACES>,
    /// The views this instance holds of somebody else's windows, which
    /// is the compositor's half and empty on everybody else.
    views: ArrayVec<(SurfaceId, ViewId), MAX_LIVE_SURFACES>,
    /// The registry to hand `held` back to. Present once this instance
    /// has created a window.
    service: Option<SurfaceService>,
}

impl SurfaceOwnership {
    pub const fn new() -> Self {
        Self {
            held: ArrayVec::new_const(),
            views: ArrayVec::new_const(),
            service: None,
        }
    }

    /// How many windows this instance owns, is creating, or handed back
    /// and the drain has not yet processed.
    pub fn held(&self) -> usize {
        self.held
            .iter()
            .filter(|shared| shared.holds_slot())
            .count()
    }

    /// Pin a frame buffer for a window of `geometry` and register the
    /// window, for showing at `span` of this instance's memory once the
    /// compositor has taken it.
    ///
    /// The span is checked before anything is pinned or registered, so a
    /// span the table would refuse costs nothing. Nothing is shown here:
    /// the caller hands [`PendingSurface::buffer`] to the compositor and
    /// then finishes with [`Self::show`] or [`Self::refused`], or drops
    /// the pending window to hand it back.
    pub fn prepare(
        &mut self,
        registry: &SurfaceService,
        views: &ViewTable,
        memory: GuestMemory,
        span: Span,
        geometry: SurfaceGeometry,
    ) -> Result<PendingSurface, SurfaceServiceError> {
        self.prune();
        if self.held.is_full() {
            return Err(SurfaceServiceError::TooManySurfaces);
        }
        let bytes = geometry
            .frame_bytes()
            .ok_or(SurfaceServiceError::UnsupportedSize)?;
        views.validate(memory, span, bytes)?;
        let buffer = Buffer::pinned(bytes)?;
        let shared = registry.register(geometry)?;
        self.service.get_or_insert_with(|| registry.clone());
        self.held.push(shared.clone());
        Ok(PendingSurface {
            shared,
            buffer,
            span,
            registry: Some(registry.clone()),
        })
    }

    /// Show a window the compositor took at the client's span, which is
    /// the last step of its create.
    ///
    /// The span is checked again: another call may have shown something
    /// overlapping it while the compositor was answering. A span that no
    /// longer takes the buffer leaves nothing shown, and the window goes
    /// back to the compositor's supervisor through the pending window's
    /// own drop, so the compositor ends its view and is told the window
    /// is gone. Its slot stays counted until that drain is done.
    pub fn show(
        &mut self,
        views: &mut ViewTable,
        memory: GuestMemory,
        pending: PendingSurface,
    ) -> Result<(Arc<SurfaceShared>, ViewId), SurfaceServiceError> {
        match views.show_buffer(memory, pending.span, &pending.buffer, None) {
            Ok(view) => Ok((pending.settle(), view)),
            Err(error) => {
                drop(pending);
                Err(error.into())
            }
        }
    }

    /// End a create the compositor refused, or never answered because
    /// it died.
    ///
    /// The compositor holds nothing of the window, so it is not told:
    /// the window comes off the desktop, and the frame buffer goes back
    /// to the pool with the pending window's reference.
    pub fn refused(&mut self, registry: &SurfaceService, pending: PendingSurface) {
        let id = pending.settle().id();
        self.abandon(registry, id);
    }

    /// Take a window this instance created off the desktop, without
    /// telling the compositor.
    ///
    /// For a window the compositor holds nothing of. The view at the
    /// client's span is the caller's to end.
    pub fn abandon(&mut self, registry: &SurfaceService, id: SurfaceId) {
        self.forget(id);
        registry.unregister(id);
    }

    /// Take a window this instance created off the desktop and hand it
    /// to the compositor's supervisor, which ends the compositor's view
    /// of it and tells the compositor it is gone.
    ///
    /// Never waits and never fails, so a resource drop can call it. The
    /// view at the client's span is the caller's to end first; the
    /// frame buffer stays pinned for as long as the compositor's view of
    /// it does. The window stays counted against this instance until the
    /// drain has processed it.
    pub fn release(&mut self, registry: &SurfaceService, id: SurfaceId) {
        registry.return_surfaces(ReturnedSurfaces::one(id));
    }

    /// The window `id` names, while this instance holds it.
    pub fn surface(&self, id: SurfaceId) -> Result<&Arc<SurfaceShared>, SurfaceServiceError> {
        self.held
            .iter()
            .find(|shared| shared.id() == id)
            .filter(|shared| shared.is_alive())
            .ok_or(SurfaceServiceError::Gone)
    }

    /// Show a frame buffer another instance pinned at `span` of this
    /// instance's memory.
    ///
    /// This is the compositor's half: the buffer is the client's, and
    /// what this records is the compositor's view of it.
    pub fn show_view(
        &mut self,
        views: &mut ViewTable,
        memory: GuestMemory,
        span: Span,
        id: SurfaceId,
        buffer: &Buffer,
    ) -> Result<(), SurfaceServiceError> {
        if self.views.is_full() {
            return Err(SurfaceServiceError::TooManySurfaces);
        }
        let view = views.show_buffer(memory, span, buffer, None)?;
        self.views.push((id, view));
        Ok(())
    }

    /// End this instance's view of one window.
    ///
    /// Doing it the moment the window is gone is what proves the
    /// compositor can no longer read the client's bytes, and it is the
    /// view's reference that keeps the client's buffer pinned. A window
    /// this instance holds no view of — one whose `create` it refused,
    /// or whose client let it go before the compositor's view was
    /// shown — ends nothing.
    ///
    /// On [`ViewError::OutOfMemory`] the view is still shown and still
    /// recorded, for the reason [`ViewTable::end`] gives.
    pub fn end_view(&mut self, id: SurfaceId, views: &mut ViewTable) -> Result<(), ViewError> {
        let Some(index) = self.views.iter().position(|(held, _)| *held == id) else {
            return Ok(());
        };
        views.end(self.views[index].1)?;
        self.views.remove(index);
        Ok(())
    }

    /// Drop `id` from the windows this instance holds.
    fn forget(&mut self, id: SurfaceId) {
        if let Some(index) = self.held.iter().position(|shared| shared.id() == id) {
            self.held.remove(index);
        }
    }

    /// Drop every window that holds no slot any more.
    ///
    /// A window the supervisor drained after the client cancelled its
    /// create — which had no store to remove the entry from — or after
    /// the resource dropped, and one retired with a compositor that
    /// died, is off the desktop and finished with.
    fn prune(&mut self) {
        self.held.retain(|shared| shared.holds_slot());
    }

    /// Hand this client's windows still on the desktop to the
    /// compositor's supervisor, if there are any to hand.
    ///
    /// A window already handed back is not handed back again: the
    /// supervisor has it, and its slot is the drain's to release.
    fn hand_back(&mut self) {
        let Some(service) = self.service.as_ref() else {
            return;
        };
        let ids = self
            .held
            .iter()
            .filter(|shared| shared.is_alive())
            .map(|shared| shared.id())
            .collect::<ArrayVec<SurfaceId, MAX_INSTANCE_SURFACES>>();
        if ids.is_empty() {
            // No window of this instance's own is on the desktop. What
            // the store may still hold is views of somebody else's
            // windows, which its view table withdraws on its own.
            return;
        }
        service.return_surfaces(ReturnedSurfaces { ids });
    }
}

impl Drop for SurfaceOwnership {
    fn drop(&mut self) {
        self.hand_back();
    }
}
