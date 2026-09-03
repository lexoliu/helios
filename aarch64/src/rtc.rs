//! PL031 real-time clock for the AArch64 backend.
//!
//! The platform exposes an ARM PL031 next to the PL011 UART. Its data
//! register holds a free-running 32-bit counter of seconds since the
//! Unix epoch, which firmware loads from the host clock, so the kernel
//! reads one register once at boot and never touches the device again.
//!
//! Concurrency contract: discovery and the single read both happen on
//! the bootstrap processor before secondaries start, and the device is
//! never interrupt-driven, so nothing here is shared.

use helios_hal::rtc::{RealTimeClock, RtcError, UnixSeconds};

use crate::LimineBootHandoff;
use crate::platform::MmioRegion;

/// `RTCDR`, the current counter value.
const PL031_DATA: usize = 0x00;

#[derive(Clone, Copy)]
pub(crate) struct Pl031Rtc {
    base: usize,
}

impl RealTimeClock for Pl031Rtc {
    const SOURCE: &'static str = "pl031";

    fn read(&self) -> Result<UnixSeconds, RtcError> {
        // SAFETY: `base` is the mapped device window the platform
        // describes for its PL031, and `RTCDR` is a 32-bit read-only
        // register at its start.
        let seconds = unsafe { ((self.base + PL031_DATA) as *const u32).read_volatile() };
        Ok(UnixSeconds::new(u64::from(seconds)))
    }
}

/// Maps the register window of the platform's PL031.
///
/// A machine that describes no PL031 has no calendar to read, and the
/// caller leaves the kernel's wall clock unseeded.
pub(crate) fn map(
    region: MmioRegion,
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
) -> Pl031Rtc {
    assert!(region.size != 0, "AArch64 PL031 window has zero size");
    crate::map_mmio_page(region.base, physical_memory_offset, handoff);
    Pl031Rtc {
        base: crate::mmio_virtual_base(region.base, physical_memory_offset),
    }
}
