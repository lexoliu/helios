//! The machine's input devices, and the instances that may read them.
//!
//! An input device reports without being asked, and a device nobody
//! reads is a device that stops working: its event ring is its whole
//! buffer pool, so once the guest has left every buffer full the host
//! has nowhere to put the next keystroke. The kernel therefore owns each
//! device it brings up and drains it for as long as the machine runs,
//! and hands the *right to read* one to exactly one instance at a time.
//!
//! That split is what the three parts here are:
//!
//! * [`owner`] holds the devices. Two tasks per device: one draining
//!   its event ring, one serving its indicators.
//! * [`service`] is how everything else reaches those tasks: one claim
//!   word per device, a bounded event queue per device, and the queue
//!   the `set-led` requests travel on. No trait object and no lock
//!   crosses that boundary.
//! * [`instance`] is the claiming instance's side: the claims its store
//!   holds, released by the same drop that kills the instance.
//!
//! # Where the events go
//!
//! Nowhere the kernel keeps. A report the device published is relayed
//! into the claiming instance's stream and forgotten; a report nobody
//! has claimed the device for is written to the kernel's log and
//! forgotten. The kernel never accumulates input.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the claim words and the queues,
//! [`owner`] for the tasks. The one rule that spans them is the
//! release: a claim is let go by a drop, which cannot await anything, so
//! the drop stores the claim word back to free and drains whatever the
//! queue still held. There is nothing to hand back — no pinned page and
//! no device resource — which is why input needs no releasing state
//! between one owner and the next.

mod instance;
mod owner;
mod service;
#[cfg(test)]
mod tests;

use thiserror::Error;

pub use instance::InputOwnership;
pub use owner::install_input_devices;
pub use service::{
    DeviceIndex, EVENT_QUEUE_DEPTH, EventBurst, InputClaim, InputDeviceSnapshot, InputEvents,
    InputLedSender, InputService, MAX_REPORT_EVENTS,
};

/// Why an input request was refused.
///
/// The variants are kept apart rather than folded into one fault because
/// a caller acts on them differently: "this machine has no input" is a
/// provisioning answer, "somebody else has that keyboard" is a
/// provisioning mistake, and "the device faulted" is something nobody
/// can retry around.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum InputServiceError {
    /// This machine has no input device, or the kernel never brought one
    /// up.
    #[error("this machine has no input device")]
    Unavailable,
    /// No device of that name on this machine.
    #[error("this machine has no input device of that name")]
    NoSuchDevice,
    /// Every device of that name is already held.
    #[error("another instance already holds every input device of that name")]
    AlreadyClaimed,
    /// A handle that outlived a release names a device its instance no
    /// longer holds.
    #[error("this instance does not hold that input device")]
    NotClaimed,
    /// This instance holds as many devices as it may.
    #[error("an instance holds at most {MAX_CLAIMED_DEVICES} input devices")]
    TooManyDevices,
    /// The device failed rather than refused.
    #[error("the input device faulted")]
    DeviceFault,
    /// The kernel's owner task stopped serving requests, which happens
    /// only when the machine is going down.
    #[error("the kernel's input owner stopped serving requests")]
    Closed,
}

/// Devices one instance may hold at once.
///
/// The bound is the machine's: the kernel routes interrupts for at most
/// [`crate::MAX_INPUT_DEVICES`] input devices, so an instance that held
/// more would be holding devices that do not exist.
pub const MAX_CLAIMED_DEVICES: usize = crate::MAX_INPUT_DEVICES;
