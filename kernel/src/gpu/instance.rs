//! One instance's hold on the machine's 3D engine.
//!
//! What an instance's store carries for the renderer is small: the
//! claim itself and the arena its command buffers and guest blobs are
//! pinned in, which is also where the host's own blob storage is
//! mapped. Both are needed together, because all of that lives *inside*
//! that instance's linear memory and the arena is what says where.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so
//! nothing here is shared and nothing here locks. The claim word and
//! the request queues behind [`Gpu3dClaim`] are the shared part and
//! carry their own synchronisation.
//!
//! # What a drop has to do, and what it must not
//!
//! Dropping this — which is what killing an instance does — has to end
//! with the renderer holding nothing of this instance's, and it cannot
//! await anything. So it does neither of the two obvious things: it
//! does not tell the device (that is asynchronous) and it does not
//! free the pages (the renderer may still be reading a guest blob).
//! It hands the arena to the owner task and raises the release signal;
//! the owner destroys the contexts, unmaps and destroys the blobs, and
//! only then lets the arena go, which is the point at which the pages
//! are the instance's pool's again.

use helios_hal::device::DeviceRegion;

use crate::device::DeviceWindow;
use crate::pins::PinnedFrame;

use super::service::{Gpu3dClaim, Gpu3dService};
use super::{Gpu3dServiceError, GpuPins};

/// One instance's side of the 3D path.
#[derive(Default)]
pub struct Gpu3dOwnership {
    /// The 3D engine, once this instance claimed it.
    claim: Option<Gpu3dClaim>,
    /// Where its command buffers, guest blobs and mapped host windows
    /// live. Present exactly when `claim` is.
    pins: Option<GpuPins>,
}

impl Gpu3dOwnership {
    pub const fn new() -> Self {
        Self {
            claim: None,
            pins: None,
        }
    }

    /// Whether this instance holds the 3D engine.
    pub const fn holds_gpu(&self) -> bool {
        self.claim.is_some()
    }

    /// The window this instance's renderer state lives in, while it
    /// holds the engine. There is no window before a claim: an instance
    /// that renders nothing is an ordinary instance and pays nothing
    /// for the path existing.
    pub fn window(&self) -> Option<DeviceWindow> {
        self.pins.as_ref().map(GpuPins::window)
    }

    /// How many bytes of this instance's memory its pinned runs and
    /// device windows hold.
    pub fn pinned_bytes(&self) -> u64 {
        self.pins.as_ref().map_or(0, GpuPins::pinned_bytes)
    }

    /// Take exclusive ownership of the machine's 3D engine, with
    /// `window` as the span of this instance's linear memory its
    /// command buffers and mapped blobs go in.
    ///
    /// Refused when this instance already holds it — a second claim
    /// would make the release ambiguous — and when somebody else does.
    pub fn claim(
        &mut self,
        service: &Gpu3dService,
        window: DeviceWindow,
    ) -> Result<(), Gpu3dServiceError> {
        if self.claim.is_some() {
            return Err(Gpu3dServiceError::AlreadyClaimed);
        }
        let claim = service.claim()?;
        self.pins = Some(GpuPins::new(window));
        self.claim = Some(claim);
        Ok(())
    }

    /// The claim, or the reason there is none.
    pub fn claim_ref(&self) -> Result<&Gpu3dClaim, Gpu3dServiceError> {
        self.claim.as_ref().ok_or(Gpu3dServiceError::NotClaimed)
    }

    /// Commit a physically contiguous run of `bytes` from this
    /// instance's pool.
    ///
    /// The one kind of pin the renderer's own memory takes: a command
    /// buffer the instance writes and the device reads, and a guest
    /// blob's backing store, are the same thing to the arena.
    pub fn pin(&mut self, bytes: u64) -> Result<PinnedFrame, Gpu3dServiceError> {
        self.pins
            .as_mut()
            .ok_or(Gpu3dServiceError::NotClaimed)?
            .pin(bytes)
            .map_err(Gpu3dServiceError::from)
    }

    /// Map `region` — a window the display engine published — into this
    /// instance's window.
    ///
    /// Nothing is allocated and nothing is charged: the bytes behind
    /// the region are the renderer's, placed in the engine's
    /// host-visible aperture, and this arena holds only the path to
    /// them.
    pub fn map_blob(&mut self, region: DeviceRegion) -> Result<PinnedFrame, Gpu3dServiceError> {
        self.pins
            .as_mut()
            .ok_or(Gpu3dServiceError::NotClaimed)?
            .map_device(region)
            .map_err(Gpu3dServiceError::from)
    }

    /// Hand one run back: unmaps a device window or releases pinned
    /// pages, whichever the frame is.
    ///
    /// Called once the renderer has let the resource go, never before:
    /// a blob unmapped while the engine still places it would leave the
    /// device decoding a span that no longer names anything.
    pub fn unpin(&mut self, frame: PinnedFrame) {
        if let Some(pins) = self.pins.as_mut() {
            pins.unpin(frame);
        }
    }

    /// Give the 3D engine back.
    ///
    /// The same path a death takes, so the two cannot diverge: the
    /// arena goes to the owner task, the claim's drop raises the
    /// release, and the pages come back to the pool once the renderer
    /// has let everything go.
    pub fn release(&mut self) {
        self.hand_back();
        self.claim = None;
    }

    /// Hand the arena to the owner task, if there is one to hand.
    fn hand_back(&mut self) {
        let (Some(claim), Some(pins)) = (self.claim.as_ref(), self.pins.take()) else {
            return;
        };
        claim.return_pins(pins);
    }
}

impl Drop for Gpu3dOwnership {
    fn drop(&mut self) {
        // Before `claim` is dropped, because dropping it is what tells
        // the owner task to look: an arena handed over afterwards would
        // arrive at a task that had already finished releasing.
        self.hand_back();
    }
}
