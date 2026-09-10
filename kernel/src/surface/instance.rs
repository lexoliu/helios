//! One instance's side of the surface path.
//!
//! Two kinds of instance carry this and they carry different halves of
//! it. A client holds the surfaces it created and the pages they live
//! in, committed from its own pool. The compositor holds a view of every
//! client's pages, mapped a second time into its own linear memory, and
//! creates no surface of its own.
//!
//! Both are the same arena in the same window, because both are the same
//! fact: pages of this instance's linear memory that the kernel placed
//! above everything the instance can grow into.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so nothing
//! here is shared and nothing here locks. The registry and the queues
//! behind [`SurfaceService`] are the shared part and carry their own
//! synchronisation.
//!
//! # What a drop has to do, and what it must not
//!
//! Dropping this — which is what killing an instance does — has to end
//! with the compositor holding no view of this instance's pages, and it
//! cannot await anything. So it does not free the pages: it hands the
//! whole arena to the compositor's supervisor and raises the return
//! signal. The supervisor takes the windows off the desktop, drops its
//! views, and only then lets the arena go, which is the point at which
//! the pages are this instance's pool's again.

use arrayvec::ArrayVec;
use triomphe::Arc;

use crate::device::DeviceWindow;
use crate::pins::{PinnedFrame, PinnedFrames};

use super::SurfaceServiceError;
use super::service::{
    MAX_INSTANCE_SURFACES, MAX_LIVE_SURFACES, ReturnedSurfaces, SurfaceGeometry, SurfaceId,
    SurfaceService, SurfaceShared,
};

/// The arena one instance pins its windows, or its views of somebody
/// else's, in.
///
/// Sized for the compositor rather than for a client: a client holds at
/// most [`MAX_INSTANCE_SURFACES`] windows, and the compositor holds a
/// view of every window on the machine.
pub type SurfacePins = PinnedFrames<MAX_LIVE_SURFACES>;

/// One instance's hold on the surface path.
#[derive(Default)]
pub struct SurfaceOwnership {
    /// Where its windows are pinned. Absent until the instance asks for
    /// its first one, because an instance that draws in no window pays
    /// nothing for the path existing.
    pins: Option<SurfacePins>,
    /// The windows this instance owns, alongside the run each lives in.
    held: ArrayVec<(Arc<SurfaceShared>, PinnedFrame), MAX_INSTANCE_SURFACES>,
    /// The registry to hand `pins` back to. Present exactly when `pins`
    /// is.
    service: Option<SurfaceService>,
}

impl SurfaceOwnership {
    pub const fn new() -> Self {
        Self {
            pins: None,
            held: ArrayVec::new_const(),
            service: None,
        }
    }

    /// The window this instance's surfaces live in, once it has one.
    pub fn window(&self) -> Option<DeviceWindow> {
        self.pins.as_ref().map(SurfacePins::window)
    }

    /// How many bytes of this instance's memory its windows hold. A view
    /// of somebody else's run counts against the window's span but not
    /// against this instance's pool, which is why the compositor's
    /// figure is address space rather than memory.
    pub fn pinned_bytes(&self) -> u64 {
        self.pins.as_ref().map_or(0, SurfacePins::pinned_bytes)
    }

    /// How many windows this instance owns right now.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The arena, opened in `window` on first use.
    fn arena<'a>(
        pins: &'a mut Option<SurfacePins>,
        service: &mut Option<SurfaceService>,
        window: DeviceWindow,
        registry: &SurfaceService,
    ) -> &'a mut SurfacePins {
        if pins.is_none() {
            *pins = Some(SurfacePins::new(window));
            *service = Some(registry.clone());
        }
        pins.as_mut()
            .expect("the arena was just opened if it was absent")
    }

    /// Commit a frame buffer for a window of `geometry` and register it.
    ///
    /// The surface exists from here on: the caller hands the identity to
    /// the compositor and, if that is refused, calls
    /// [`Self::abandon`] to take it back off the desktop.
    pub fn create(
        &mut self,
        registry: &SurfaceService,
        window: DeviceWindow,
        geometry: SurfaceGeometry,
    ) -> Result<(Arc<SurfaceShared>, PinnedFrame), SurfaceServiceError> {
        if self.held.is_full() {
            return Err(SurfaceServiceError::TooManySurfaces);
        }
        let bytes = geometry
            .frame_bytes()
            .ok_or(SurfaceServiceError::UnsupportedSize)?;
        let arena = Self::arena(&mut self.pins, &mut self.service, window, registry);
        let frame = arena.pin(bytes)?;
        let shared = match registry.register(geometry) {
            Ok(shared) => shared,
            Err(error) => {
                arena.unpin(frame);
                return Err(error);
            }
        };
        self.held.push((shared.clone(), frame));
        Ok((shared, frame))
    }

    /// Take a window this instance just created back off the desktop.
    ///
    /// The run stays pinned, for the reason [`super`] states: a span
    /// released while the compositor may still hold a view of it would
    /// put whatever the pool hands out next in front of the desktop.
    pub fn abandon(&mut self, registry: &SurfaceService, id: SurfaceId) {
        if let Some(index) = self.held.iter().position(|(shared, _)| shared.id() == id) {
            self.held.remove(index);
        }
        registry.unregister(id);
    }

    /// The window `id` names, while this instance holds it.
    pub fn surface(&self, id: SurfaceId) -> Result<&Arc<SurfaceShared>, SurfaceServiceError> {
        self.held
            .iter()
            .find(|(shared, _)| shared.id() == id)
            .map(|(shared, _)| shared)
            .filter(|shared| shared.is_alive())
            .ok_or(SurfaceServiceError::Gone)
    }

    /// Map a run another instance committed into this instance's window.
    ///
    /// This is the compositor's half: the run is the client's, and what
    /// this arena holds is the view of it.
    pub fn map_view(
        &mut self,
        registry: &SurfaceService,
        window: DeviceWindow,
        physical: helios_hal::iommu::PhysicalRange,
    ) -> Result<PinnedFrame, SurfaceServiceError> {
        let arena = Self::arena(&mut self.pins, &mut self.service, window, registry);
        Ok(arena.map(physical)?)
    }

    /// Drop one view this instance holds.
    pub fn unmap_view(&mut self, frame: PinnedFrame) {
        if let Some(pins) = self.pins.as_mut() {
            pins.unpin(frame);
        }
    }

    /// Hand the arena to the compositor's supervisor, if there is one to
    /// hand.
    fn hand_back(&mut self) {
        if self.held.is_empty() {
            // Nothing of this instance's own is pinned. What the arena
            // may still hold is views of somebody else's pages, which
            // the compositor's store carries and which its own drop
            // unmaps: those free nothing and need nobody's ordering.
            return;
        }
        let (Some(service), Some(pins)) = (self.service.as_ref(), self.pins.take()) else {
            return;
        };
        let ids = self
            .held
            .drain(..)
            .map(|(shared, _)| shared.id())
            .collect::<ArrayVec<SurfaceId, MAX_INSTANCE_SURFACES>>();
        service.return_surfaces(ReturnedSurfaces { ids, pins });
    }
}

impl Drop for SurfaceOwnership {
    fn drop(&mut self) {
        self.hand_back();
    }
}
