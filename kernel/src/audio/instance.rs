//! One instance's hold on a playback stream.
//!
//! What an instance's store carries for audio is the claim itself and
//! the ring its period buffers are handed to the device through. The
//! period buffers are [`Buffer`]s pinned from the instance's own pool;
//! they are shown in no instance's memory, because the samples reach
//! them through the guest's `samples` stream and are copied in by the
//! kernel through each buffer's kernel alias.
//!
//! An instance holds one playback stream at a time. A second claim
//! would make the release ambiguous, so it is refused. A machine with
//! two sound cards hands its second stream to a second instance.
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
//! await anything. So it does not tell the device (that is
//! asynchronous): it closes the ring, so the playback task's pump ends,
//! and raises the release. The ring holds the period buffers, and the
//! playback task holds the ring for as long as it is playing from it, so
//! the buffers go back to the pool when the task lets the ring go —
//! after the device has stopped and released the stream — whatever
//! order the instance's own side went in.

use arrayvec::ArrayVec;
use helios_hal::audio::{PcmParams, StreamDirection};
use triomphe::Arc;

use crate::buffer::Buffer;
use crate::process::AudioClaimCap;

use super::AudioServiceError;
use super::service::{AudioClaim, AudioService, PERIODS_IN_FLIGHT, PeriodRing, PlaybackFormat};

/// One instance's side of the audio path.
#[derive(Default)]
pub struct AudioOwnership {
    /// The stream, once this instance claimed one.
    claim: Option<AudioClaim>,
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
            ring: None,
            format: None,
        }
    }

    /// Whether this instance holds a playback stream.
    pub const fn holds_stream(&self) -> bool {
        self.claim.is_some()
    }

    /// The format this claim agreed, if it has.
    pub const fn format(&self) -> Option<PlaybackFormat> {
        self.format
    }

    /// Take exclusive ownership of the stream `id` names.
    pub fn claim(
        &mut self,
        service: &AudioService,
        id: u32,
        cap: AudioClaimCap,
    ) -> Result<(), AudioServiceError> {
        if self.claim.is_some() {
            return Err(AudioServiceError::AlreadyClaimed);
        }
        self.claim = Some(service.claim(id, cap)?);
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
    /// The buffers are pinned inside the host call that asked, so they
    /// are charged to this instance. What comes back is the ring and the
    /// parameters the device is to be configured with. Telling the
    /// device is the playback task's work, so a negotiation it refuses
    /// is undone with [`Self::discard_negotiation`].
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
        // A buffer that cannot be pinned drops the ones already pinned
        // on the way out, so a refused negotiation holds nothing.
        let mut periods = ArrayVec::<Buffer, PERIODS_IN_FLIGHT>::new();
        for _ in 0..PERIODS_IN_FLIGHT {
            periods.push(Buffer::pinned(u64::from(params.period_bytes))?);
        }
        let ring = Arc::new(PeriodRing::new(periods, params.period_bytes));
        self.ring = Some(ring.clone());
        self.format = Some(format);
        Ok((ring, params))
    }

    /// Forget everything the last [`Self::negotiate`] agreed.
    ///
    /// Called when the playback task refuses the parameters, which is
    /// the one point at which nothing has been handed to the device yet:
    /// the period buffers go back with the last reference to the ring.
    pub fn discard_negotiation(&mut self) {
        self.ring = None;
        self.format = None;
    }

    /// Give the stream back.
    ///
    /// The same path a death takes, so the two cannot diverge: the ring
    /// closes, the claim's drop raises the release, and the period
    /// buffers come back to the pool once the playback task has stopped
    /// the device reading them and let the ring go.
    pub fn release(&mut self) {
        self.close_ring();
        self.claim = None;
        self.format = None;
    }

    /// Close the ring, if there is one, and forget it.
    fn close_ring(&mut self) {
        if let Some(ring) = self.ring.take() {
            // The pump ends when the ring closes, and it is the pump
            // that is still handing these periods to the device.
            ring.close();
        }
    }
}

impl Drop for AudioOwnership {
    fn drop(&mut self) {
        // Before `claim` is dropped, because dropping it is what tells
        // the playback task to look, and the pump has to see the ring
        // closed when it does.
        self.close_ring();
    }
}
