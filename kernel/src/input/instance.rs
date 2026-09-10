//! One instance's hold on the machine's input devices.
//!
//! What an instance's store carries for input is the claims themselves
//! and nothing else. Unlike the display, input puts nothing inside the
//! instance's linear memory: an event is eight bytes the kernel relays
//! into a stream, so there is no window to reserve, no page to pin and
//! no growth to cap.
//!
//! An instance may hold several devices at once, because a compositor
//! needs all of them — a keyboard and a pointer are one desktop — and
//! holding them separately is what lets the kernel keep serving the ones
//! nobody claimed.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so nothing
//! here is shared and nothing here locks. The claim words and the event
//! queues behind [`InputClaim`] are the shared part and carry their own
//! synchronisation.
//!
//! # What a drop has to do
//!
//! Dropping this — which is what killing an instance does — has to end
//! with every device this instance held back under the kernel's own
//! drain, and it cannot await anything. It does not have to: releasing
//! an input device is one store to its claim word plus emptying a queue,
//! both of which [`InputClaim`]'s own drop does.

use arrayvec::ArrayVec;

use super::service::{DeviceIndex, InputClaim, InputService};
use super::{InputServiceError, MAX_CLAIMED_DEVICES};

/// One instance's side of the input path.
#[derive(Default)]
pub struct InputOwnership {
    claims: ArrayVec<InputClaim, MAX_CLAIMED_DEVICES>,
}

impl InputOwnership {
    pub const fn new() -> Self {
        Self {
            claims: ArrayVec::new_const(),
        }
    }

    /// How many devices this instance holds.
    pub const fn held(&self) -> usize {
        self.claims.len()
    }

    /// Take exclusive ownership of the device `name` names.
    ///
    /// Refused when this instance already holds as many devices as the
    /// machine has, and when every device of that name is held by
    /// somebody. Claiming the same name twice on a machine with two
    /// devices of that name yields both.
    pub fn claim(
        &mut self,
        service: &InputService,
        name: &str,
    ) -> Result<&InputClaim, InputServiceError> {
        if self.claims.is_full() {
            return Err(InputServiceError::TooManyDevices);
        }
        let claim = service.claim(name)?;
        self.claims.push(claim);
        Ok(self
            .claims
            .last()
            .expect("a claim was just pushed onto this instance's list"))
    }

    /// The claim on `index`, checked against the generation the caller's
    /// handle was built with.
    ///
    /// A handle that outlived its claim names a device its instance no
    /// longer holds, and answering it against whoever holds the device
    /// now would let a dead compositor read a live one's keyboard.
    pub fn claim_ref(
        &self,
        index: DeviceIndex,
        generation: u64,
    ) -> Result<&InputClaim, InputServiceError> {
        self.claims
            .iter()
            .find(|claim| claim.index() == index && claim.generation() == generation)
            .ok_or(InputServiceError::NotClaimed)
    }

    /// Give one device back.
    ///
    /// The same path a death takes, so the two cannot diverge: the claim
    /// is dropped, which stores its word back to free and throws away
    /// whatever this instance never read.
    pub fn release(&mut self, index: DeviceIndex, generation: u64) {
        self.claims
            .retain(|claim| claim.index() != index || claim.generation() != generation);
    }
}
