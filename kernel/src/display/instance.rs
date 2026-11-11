//! One instance's hold on the machine's display.
//!
//! What an instance's store carries for the display is the claim and
//! nothing else. A surface's frame buffer is a [`crate::Buffer`] the
//! surface's resource, its view and the owner task's record of the
//! device resource each hold a reference to, so none of them has to be
//! accounted for here.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so nothing
//! here is shared and nothing here locks. The claim word and the request
//! queues behind [`DisplayClaim`] are the shared part and carry their
//! own synchronisation.
//!
//! # What a drop has to do, and what it must not
//!
//! Dropping this — which is what killing an instance does — has to end
//! with the display engine holding nothing of this instance's, and it
//! cannot await anything. So it does not tell the device (that is
//! asynchronous): it raises the release, and the owner task blanks the
//! outputs, drops the resources and, with them, its references to the
//! frame buffers. A frame's run goes back to the instance's pool when
//! the last of those references does — after the display engine has let
//! go of it, whatever order the instance's own side went in.

use super::DisplayServiceError;
use super::service::{DisplayClaim, DisplayService};

/// One instance's side of the display path.
#[derive(Default)]
pub struct DisplayOwnership {
    /// The display, once this instance claimed it.
    claim: Option<DisplayClaim>,
}

impl DisplayOwnership {
    pub const fn new() -> Self {
        Self { claim: None }
    }

    /// Whether this instance holds the display.
    pub const fn holds_display(&self) -> bool {
        self.claim.is_some()
    }

    /// Take exclusive ownership of the display.
    ///
    /// Refused when this instance already holds it — a second claim
    /// would make the release ambiguous — and when somebody else does.
    pub fn claim(&mut self, service: &DisplayService) -> Result<(), DisplayServiceError> {
        if self.claim.is_some() {
            return Err(DisplayServiceError::AlreadyClaimed);
        }
        self.claim = Some(service.claim()?);
        Ok(())
    }

    /// The claim, or the reason there is none.
    pub fn claim_ref(&self) -> Result<&DisplayClaim, DisplayServiceError> {
        self.claim.as_ref().ok_or(DisplayServiceError::NotClaimed)
    }

    /// Give the display back.
    ///
    /// The same path a death takes, so the two cannot diverge: the
    /// claim's drop raises the release, and the frame buffers go back to
    /// the pool once the display engine has stopped reading them and the
    /// instance's own handles on them are gone.
    pub fn release(&mut self) {
        self.claim = None;
    }
}
