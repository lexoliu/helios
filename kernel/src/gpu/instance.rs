//! One instance's hold on the machine's 3D engine.
//!
//! What an instance's store carries for the renderer is small: the claim
//! itself and the set of views — command buffers, guest blobs, aperture
//! placements — that ends when the claim does. The views live in the
//! store's [`ViewTable`]; the buffers behind them are held by the
//! handles that name them and by the requests that have the device read
//! them.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so
//! nothing here is shared and nothing here locks. The claim word and
//! the request queues behind [`Gpu3dClaim`] are the shared part and
//! carry their own synchronisation.
//!
//! # What a release has to do, and what it must not
//!
//! Giving the engine back while the instance lives ends every view of
//! the claim first: an aperture placement must not stay shown in the
//! instance's memory once the owner task takes the blob out of the
//! aperture, and a buffer shown nowhere goes back to the pool as soon as
//! the device has let go of it. Dying ends them the same way, earlier:
//! the store's view table is dropped before this, so every span is
//! withdrawn by the time the claim's drop raises the release. Neither
//! path awaits the device; the owner task destroys the contexts, unmaps
//! and destroys the blobs, and drops its references to their buffers.

use crate::buffer::{ViewError, ViewGroup, ViewTable};

use super::{Gpu3dServiceError, MAX_GPU_VIEWS};

use super::service::{Gpu3dClaim, Gpu3dService};

/// One instance's side of the 3D path.
#[derive(Default)]
pub struct Gpu3dOwnership {
    /// The 3D engine, once this instance claimed it.
    claim: Option<Gpu3dClaim>,
    /// The set of views that ends when the claim does. Present exactly
    /// when `claim` is.
    views: Option<ViewGroup>,
}

impl Gpu3dOwnership {
    pub const fn new() -> Self {
        Self {
            claim: None,
            views: None,
        }
    }

    /// Whether this instance holds the 3D engine.
    pub const fn holds_gpu(&self) -> bool {
        self.claim.is_some()
    }

    /// Take exclusive ownership of the machine's 3D engine, with `views`
    /// as the set every view the claim shows goes in.
    ///
    /// Refused when this instance already holds it — a second claim
    /// would make the release ambiguous — and when somebody else does.
    pub fn claim(
        &mut self,
        service: &Gpu3dService,
        views: ViewGroup,
    ) -> Result<(), Gpu3dServiceError> {
        if self.claim.is_some() {
            return Err(Gpu3dServiceError::AlreadyClaimed);
        }
        self.claim = Some(service.claim()?);
        self.views = Some(views);
        Ok(())
    }

    /// The claim, or the reason there is none.
    pub fn claim_ref(&self) -> Result<&Gpu3dClaim, Gpu3dServiceError> {
        self.claim.as_ref().ok_or(Gpu3dServiceError::NotClaimed)
    }

    /// The set the claim's views go in, or the reason there is none.
    pub fn views(&self) -> Result<ViewGroup, Gpu3dServiceError> {
        self.views.ok_or(Gpu3dServiceError::NotClaimed)
    }

    /// The set the claim's next view goes in, refused once the claim
    /// shows [`MAX_GPU_VIEWS`]: the claim's share of the store's table,
    /// which a plugin creating command buffers without bound would
    /// otherwise take from the store's other paths.
    pub fn room_for_view(&self, table: &ViewTable) -> Result<ViewGroup, Gpu3dServiceError> {
        let group = self.views()?;
        if table.group_len(group) >= MAX_GPU_VIEWS {
            return Err(Gpu3dServiceError::TooManyViews);
        }
        Ok(group)
    }

    /// Give the 3D engine back.
    ///
    /// Every view of the claim ends first, then the claim's drop raises
    /// the release. Fails, holding on to the claim, only when a view
    /// cannot be ended for want of memory to put back.
    pub fn release(&mut self, views: &mut ViewTable) -> Result<(), ViewError> {
        if let Some(group) = self.views {
            views.end_group(group)?;
        }
        self.views = None;
        self.claim = None;
        Ok(())
    }
}
