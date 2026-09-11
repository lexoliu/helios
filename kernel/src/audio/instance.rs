//! One instance's hold on a playback stream.
//!
//! What an instance's store carries for audio is the claim itself, the
//! arena its period buffers are pinned in, and the ring those buffers
//! are handed to the device through. All three are needed together,
//! because the buffers live *inside* that instance's linear memory and
//! the arena is what says where.
//!
//! An instance holds one playback stream at a time. A second claim
//! would need a second arena in the same window — two bump cursors into
//! one span — and would make the release ambiguous, so it is refused.
//! A machine with two sound cards hands its second stream to a second
//! instance.
//!
//! # Concurrency contract
//!
//! Store state is owned by the one task running the instance, so
//! nothing here is shared and nothing here locks. The claim word, the
//! request queue and the period ring behind [`AudioClaim`] are the
//! shared part and carry their own synchronisation.
//!
//! # What a drop has to do, and what it must not
//!
//! Dropping this — which is what killing an instance does — has to end
//! with the device holding nothing of this instance's, and it cannot
//! await anything. So it does neither of the two obvious things: it does
//! not tell the device (that is asynchronous) and it does not free the
//! pages (the device may still be reading them). It closes the ring, so
//! the playback task's pump ends, hands the arena to that task and
//! raises the release; the task stops the stream, lets the device
//! release what it allocated, and only then lets the arena go, which is
//! the point at which the pages are the instance's pool's again.

use arrayvec::ArrayVec;
use helios_hal::audio::{PcmParams, StreamDirection};
use triomphe::Arc;

use crate::device::DeviceWindow;
use crate::pins::{PinnedArena, PinnedRun};

use super::AudioServiceError;
use super::service::{AudioClaim, AudioService, PERIODS_IN_FLIGHT, PeriodRing, PlaybackFormat};

/// The audio window of one instance, as a bump arena.
///
/// A period buffer is the claiming instance's own memory — pinned,
/// physically contiguous pages committed from the user pool and placed
/// at a fixed offset inside that instance's linear memory — so it is
/// [`PinnedArena`] with audio's own bound on how many runs one claim
/// may hold, which is the number of periods the kernel keeps in flight.
pub type AudioPins = PinnedArena<PERIODS_IN_FLIGHT>;

/// One instance's side of the audio path.
#[derive(Default)]
pub struct AudioOwnership {
    /// The stream, once this instance claimed one.
    claim: Option<AudioClaim>,
    /// Where its period buffers are pinned. Present exactly when
    /// `claim` is.
    pins: Option<AudioPins>,
    /// The buffers themselves, in the order they were pinned, so a
    /// negotiation the device refused can be undone in reverse and give
    /// its whole span back.
    runs: ArrayVec<PinnedRun, PERIODS_IN_FLIGHT>,
    /// The ring the samples cross in, once a format has been agreed.
    ring: Option<Arc<PeriodRing>>,
    /// The format that was agreed. A stream is negotiated once per
    /// claim.
    format: Option<PlaybackFormat>,
}

impl AudioOwnership {
    pub const fn new() -> Self {
        Self {
            claim: None,
            pins: None,
            runs: ArrayVec::new_const(),
            ring: None,
            format: None,
        }
    }

    /// Whether this instance holds a playback stream.
    pub const fn holds_stream(&self) -> bool {
        self.claim.is_some()
    }

    /// The window this instance's period buffers live in, while it
    /// holds a stream. There is no window before a claim: an instance
    /// that plays nothing is an ordinary instance and pays nothing for
    /// the path existing.
    pub fn window(&self) -> Option<DeviceWindow> {
        self.pins.as_ref().map(AudioPins::window)
    }

    /// How many bytes of this instance's memory its period buffers
    /// hold.
    pub fn pinned_bytes(&self) -> u64 {
        self.pins.as_ref().map_or(0, AudioPins::pinned_bytes)
    }

    /// The format this claim agreed, if it has.
    pub const fn format(&self) -> Option<PlaybackFormat> {
        self.format
    }

    /// Take exclusive ownership of the stream `id` names, with `window`
    /// as the span of this instance's linear memory its period buffers
    /// go in.
    pub fn claim(
        &mut self,
        service: &AudioService,
        id: u32,
        window: DeviceWindow,
    ) -> Result<(), AudioServiceError> {
        if self.claim.is_some() {
            return Err(AudioServiceError::AlreadyClaimed);
        }
        let claim = service.claim(id)?;
        self.pins = Some(AudioPins::new(window));
        self.claim = Some(claim);
        Ok(())
    }

    /// The claim, or the reason there is none.
    pub fn claim_ref(&self) -> Result<&AudioClaim, AudioServiceError> {
        self.claim.as_ref().ok_or(AudioServiceError::NotClaimed)
    }

    /// The ring this claim's samples cross in, or the reason there is
    /// none.
    pub fn ring(&self) -> Result<Arc<PeriodRing>, AudioServiceError> {
        self.ring.clone().ok_or(AudioServiceError::NotNegotiated)
    }

    /// Agree `format` for this claim and pin the period buffers it
    /// needs.
    ///
    /// The format is checked against what the device said the stream
    /// accepts and refused outright when it does not: a caller handed
    /// the nearest thing the device does take would play its samples at
    /// the wrong speed and hear a device fault rather than its own
    /// mistake.
    ///
    /// What comes back is the ring and the parameters the device is to
    /// be configured with. Telling the device is the playback task's
    /// work, so a negotiation it refuses is undone with
    /// [`Self::discard_negotiation`].
    pub fn negotiate(
        &mut self,
        format: PlaybackFormat,
    ) -> Result<(Arc<PeriodRing>, PcmParams), AudioServiceError> {
        let claim = self.claim_ref()?;
        if self.format.is_some() {
            return Err(AudioServiceError::AlreadyNegotiated);
        }
        let info = *claim.info();
        if info.direction != StreamDirection::Playback {
            return Err(AudioServiceError::NotPlayback);
        }
        if !format.accepted_by(&info) {
            return Err(AudioServiceError::UnsupportedFormat);
        }
        let params = format
            .pcm_params()
            .ok_or(AudioServiceError::UnsupportedFormat)?;
        let pins = self.pins.as_mut().ok_or(AudioServiceError::NotClaimed)?;
        for _ in 0..PERIODS_IN_FLIGHT {
            match pins.pin(u64::from(params.period_bytes)) {
                Ok(run) => self.runs.push(run),
                Err(error) => {
                    // Undo in reverse, so the whole negotiation's span
                    // goes back to the arena rather than being held
                    // until the claim ends.
                    unpin_all(pins, &mut self.runs);
                    return Err(AudioServiceError::from(error));
                }
            }
        }
        let ring = Arc::new(PeriodRing::new(&self.runs, params.period_bytes));
        self.ring = Some(ring.clone());
        self.format = Some(format);
        Ok((ring, params))
    }

    /// Give back everything the last [`Self::negotiate`] pinned.
    ///
    /// Called when the playback task refuses the parameters, which is
    /// the one point at which nothing has been handed to the device yet
    /// and the pages can go straight back.
    pub fn discard_negotiation(&mut self) {
        self.ring = None;
        self.format = None;
        if let Some(pins) = self.pins.as_mut() {
            unpin_all(pins, &mut self.runs);
        }
    }

    /// Give the stream back.
    ///
    /// The same path a death takes, so the two cannot diverge: the ring
    /// closes, the arena goes to the playback task, the claim's drop
    /// raises the release, and the pages come back to the pool once the
    /// device has stopped reading them.
    pub fn release(&mut self) {
        self.hand_back();
        self.claim = None;
        self.format = None;
    }

    /// Close the ring, hand the arena to the playback task and forget
    /// the runs, if there is anything to hand.
    fn hand_back(&mut self) {
        if let Some(ring) = self.ring.take() {
            // Before the arena moves: the pump ends when the ring
            // closes, and it is the pump that is still handing these
            // pages to the device.
            ring.close();
        }
        self.runs.clear();
        let (Some(claim), Some(pins)) = (self.claim.as_ref(), self.pins.take()) else {
            return;
        };
        claim.return_pins(pins);
    }
}

impl Drop for AudioOwnership {
    fn drop(&mut self) {
        // Before `claim` is dropped, because dropping it is what tells
        // the playback task to look: an arena handed over afterwards
        // would arrive at a task that had already finished releasing.
        self.hand_back();
    }
}

fn unpin_all(pins: &mut AudioPins, runs: &mut ArrayVec<PinnedRun, PERIODS_IN_FLIGHT>) {
    while let Some(run) = runs.pop() {
        pins.unpin(run);
    }
}
