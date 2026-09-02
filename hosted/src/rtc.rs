//! The host operating system as the hosted backend's calendar.
//!
//! A hosted kernel has no device to program: the host's clock is the
//! platform's real-time clock, and it is always there. It is still read
//! exactly once, at bring-up, so the hosted backend exercises the same
//! seeding path the bare-metal ones do.

use std::time::{SystemTime, UNIX_EPOCH};

use helios_hal::rtc::{RealTimeClock, RtcError, UnixSeconds};

#[derive(Clone, Copy)]
pub(crate) struct HostRtc;

impl RealTimeClock for HostRtc {
    const SOURCE: &'static str = "host-clock";

    fn read(&self) -> Result<UnixSeconds, RtcError> {
        let since_epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|error| panic!("the host clock precedes the Unix epoch: {error}"));
        Ok(UnixSeconds::new(since_epoch.as_secs()))
    }
}
