//! One instance's hold on the machine's display.
//!
//! What an instance's store carries for the display is small: the claim
//! itself and the arena its frame buffers are pinned in. Both are needed
//! together, because the frame buffers live *inside* that instance's
//! linear memory and the arena is what says where.
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
//! cannot await anything. So it does neither of the two obvious things:
//! it does not tell the device (that is asynchronous) and it does not
//! free the pages (the device may still be scanning them out). It hands
//! the arena to the owner task and raises the release signal; the owner
//! blanks the outputs, drops the resources, and only then lets the arena
//! go, which is the point at which the pages are the instance's pool's
//! again.

use helios_hal::display::DisplayMode;

use crate::device::DeviceWindow;

use super::DisplayServiceError;
use super::service::{DisplayClaim, DisplayService};
use super::{DisplayPins, PinnedFrame};

/// One instance's side of the display path.
#[derive(Default)]
pub struct DisplayOwnership {
    /// The display, once this instance claimed it.
    claim: Option<DisplayClaim>,
    /// Where its frame buffers are pinned. Present exactly when
    /// `claim` is.
    pins: Option<DisplayPins>,
}

impl DisplayOwnership {
    pub const fn new() -> Self {
        Self {
            claim: None,
            pins: None,
        }
    }

    /// Whether this instance holds the display.
    pub const fn holds_display(&self) -> bool {
        self.claim.is_some()
    }

    /// The window this instance's frame buffers live in, while it holds
    /// the display. There is no window before a claim: an instance that
    /// draws nothing is an ordinary instance and pays nothing for the
    /// path existing.
    pub fn window(&self) -> Option<DeviceWindow> {
        self.pins.as_ref().map(DisplayPins::window)
    }

    /// How many bytes of this instance's memory its frame buffers hold.
    pub fn pinned_bytes(&self) -> u64 {
        self.pins.as_ref().map_or(0, DisplayPins::pinned_bytes)
    }

    /// Take exclusive ownership of the display, with `window` as the
    /// span of this instance's linear memory its frame buffers go in.
    ///
    /// Refused when this instance already holds it — a second claim
    /// would make the release ambiguous — and when somebody else does.
    pub fn claim(
        &mut self,
        service: &DisplayService,
        window: DeviceWindow,
    ) -> Result<(), DisplayServiceError> {
        if self.claim.is_some() {
            return Err(DisplayServiceError::AlreadyClaimed);
        }
        let claim = service.claim()?;
        self.pins = Some(DisplayPins::new(window));
        self.claim = Some(claim);
        Ok(())
    }

    /// The claim, or the reason there is none.
    pub fn claim_ref(&self) -> Result<&DisplayClaim, DisplayServiceError> {
        self.claim.as_ref().ok_or(DisplayServiceError::NotClaimed)
    }

    /// Commit a frame buffer large enough for one frame of `mode` in
    /// `bytes_per_pixel`-wide pixels.
    pub fn pin_frame(
        &mut self,
        mode: DisplayMode,
        bytes_per_pixel: usize,
    ) -> Result<PinnedFrame, DisplayServiceError> {
        let bytes = (mode.width as u64)
            .checked_mul(mode.height as u64)
            .and_then(|pixels| pixels.checked_mul(bytes_per_pixel as u64))
            .ok_or(DisplayServiceError::UnsupportedMode)?;
        self.pins
            .as_mut()
            .ok_or(DisplayServiceError::NotClaimed)?
            .pin(bytes)
            .map_err(DisplayServiceError::from)
    }

    /// Hand one frame buffer's pages back to this instance's pool.
    ///
    /// Called once the display engine has let the resource go, never
    /// before: pages released while a scanout still latched them would
    /// put whatever the pool hands out next on the screen.
    pub fn unpin_frame(&mut self, frame: PinnedFrame) {
        if let Some(pins) = self.pins.as_mut() {
            pins.unpin(frame);
        }
    }

    /// Give the display back.
    ///
    /// The same path a death takes, so the two cannot diverge: the arena
    /// goes to the owner task, the claim's drop raises the release, and
    /// the pages come back to the pool once the display engine has
    /// stopped reading them.
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

impl Drop for DisplayOwnership {
    fn drop(&mut self) {
        // Before `claim` is dropped, because dropping it is what tells
        // the owner task to look: an arena handed over afterwards would
        // arrive at a task that had already finished releasing.
        self.hand_back();
    }
}
