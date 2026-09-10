//! The machine's sound device, and the instances that may play down it.
//!
//! A sound device reports without being asked — a period elapsed, an
//! underrun, a plug pulled out of a jack — and a device nobody reads is
//! a device that stops reporting: its event ring is its whole buffer
//! pool, so once the guest has left every buffer full the host has
//! nowhere to put the next announcement, and on a transport whose
//! interrupt line is a function of a read-to-clear status register the
//! line never falls again either. The kernel therefore owns the device
//! for as long as the machine runs, and hands the *right to play down*
//! one of its streams to exactly one instance at a time.
//!
//! That split is what the four parts here are:
//!
//! * [`owner`] holds the device. One task draining its event ring, and
//!   one task per playback stream that configures the stream and keeps
//!   its transmit ring fed.
//! * [`service`] is how everything else reaches those tasks: one claim
//!   word per stream, the queue a claim's requests travel on, the ring
//!   of period buffers the samples cross in, and the bounded queue the
//!   feedback comes back on. No trait object and no lock crosses that
//!   boundary.
//! * [`instance`] is the claiming instance's side: the claim its store
//!   holds and the arena its period buffers are pinned in, released by
//!   the same drop that kills the instance.
//!
//! # Where the samples are
//!
//! Nowhere the kernel owns. A stream's period buffers are committed
//! from the user pool into the claiming instance's audio window —
//! pinned, physically contiguous pages accounted to that instance — and
//! those same pages are what the device is told to read. The bytes
//! arriving on the guest's `samples` stream are copied into them once,
//! by the task that owns the guest's store, and the device takes them
//! from there.
//!
//! # What the kernel never plays
//!
//! Silence it made up. A stream whose producer has not supplied the
//! next period is a stream that runs dry, and the device says so: the
//! underrun becomes an `xrun` on the claim's feedback stream and
//! nothing is substituted, because a consumer that wants silence can
//! send silence and one that does not wants to know. The only zeros the
//! kernel ever adds are the tail of a final period, without which the
//! samples before them could not be handed to a device that takes whole
//! periods.
//!
//! # Concurrency contract
//!
//! Stated per part: [`service`] for the claim words, the ring and the
//! queues, [`owner`] for the tasks. The one rule that spans them is the
//! release: a claim is let go by a drop, which cannot await anything,
//! so the stream is neither held nor free until the stream's own task
//! has stopped the device and taken the pages back — and the pages are
//! handed to that task rather than freed by the drop, because the
//! device may still be reading them when the instance dies.

mod instance;
mod owner;
mod service;
#[cfg(test)]
mod tests;

use thiserror::Error;

use crate::pins::PinError;

pub use instance::AudioOwnership;
pub use owner::install_audio_device;
pub use service::{
    AudioClaim, AudioSender, AudioService, AudioStreamSnapshot, FEEDBACK_QUEUE_DEPTH, Feedback,
    FeedbackBurst, FeedbackReader, PERIOD_MICROS, PERIODS_IN_FLIGHT, PeriodRing, PeriodWriter,
    PlaybackFormat,
};

/// Why an audio request was refused.
///
/// The variants are kept apart rather than folded into one fault
/// because a player acts on them differently: "this machine has no
/// sound device" is a provisioning answer, "somebody else has that
/// stream" is a provisioning mistake, "this device does not take that
/// rate" is something the caller can retry with another format, and
/// "the device faulted" is something nobody can retry around.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum AudioServiceError {
    /// This machine has no sound device, or the kernel has not finished
    /// bringing one up.
    #[error("this machine has no sound device")]
    Unavailable,
    /// No stream of that id on this device.
    #[error("this sound device has no stream of that id")]
    NoSuchStream,
    /// The stream exists and records rather than plays.
    #[error("that stream captures; it cannot be played to")]
    NotPlayback,
    /// Another instance holds that stream, or this instance already
    /// holds one, or the last owner's resources have not been handed
    /// back yet.
    #[error("another instance already holds that playback stream")]
    AlreadyClaimed,
    /// A handle that outlived a release names a stream its instance no
    /// longer holds.
    #[error("this instance does not hold that playback stream")]
    NotClaimed,
    /// This claim has already agreed a format.
    ///
    /// A stream is negotiated once: its period buffers are pinned then,
    /// and a second format would need the device to have stopped
    /// reading the first set, which is what releasing the claim is.
    #[error("this playback stream has already agreed a format")]
    AlreadyNegotiated,
    /// Samples were offered to a stream whose format has not been
    /// agreed. Nothing says how long a period is, so nothing can be
    /// played.
    #[error("this playback stream has no negotiated format")]
    NotNegotiated,
    /// The rate, channel count or sample layout is not one this stream
    /// accepts.
    ///
    /// Refused rather than answered with the nearest thing the device
    /// does take: a caller handed a format it did not ask for would
    /// play its samples at the wrong speed and hear a device fault
    /// rather than its own mistake.
    #[error("this stream does not accept that format")]
    UnsupportedFormat,
    /// The period buffers could not be pinned: the machine has no
    /// contiguous run of that size left, or the instance's own memory
    /// accounting refused it.
    #[error("no contiguous run of memory left for a period buffer")]
    OutOfMemory,
    /// The instance's audio window has no room left.
    #[error("this instance's audio window has no room left")]
    WindowExhausted,
    /// The device failed rather than refused.
    #[error("the sound device faulted")]
    DeviceFault,
    /// The kernel's audio owner stopped serving requests, which happens
    /// only when the machine is going down.
    #[error("the kernel's audio owner stopped serving requests")]
    Closed,
}

/// A period buffer the arena refused, in the vocabulary a player acts
/// on.
impl From<PinError> for AudioServiceError {
    fn from(error: PinError) -> Self {
        match error {
            // A period of no bytes comes from a format whose frame size
            // is zero, which is a format no stream accepts.
            PinError::EmptyRun => Self::UnsupportedFormat,
            PinError::TooManyRuns | PinError::WindowExhausted => Self::WindowExhausted,
            PinError::OutOfMemory => Self::OutOfMemory,
            // A playback claim never asks for a view of somebody else's
            // run, so an address space refusing one here is a wiring
            // fault rather than a shortage.
            PinError::ShareRefused => Self::DeviceFault,
        }
    }
}
