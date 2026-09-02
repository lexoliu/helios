//! Goldfish real-time clock for the RISC-V backend.
//!
//! The virt board exposes the Android goldfish RTC, whose two 32-bit
//! registers hold nanoseconds since the Unix epoch. Reading `TIME_LOW`
//! latches the high half, so the pair has to be read low first to see
//! one instant rather than two. The kernel reads it once at boot and
//! never touches the device again.
//!
//! Concurrency contract: discovery and the single read both happen on
//! the bootstrap hart before secondaries start, and the device is never
//! interrupt-driven, so the latch is not shared.

use fdt::Fdt;
use helios_hal::rtc::{NANOS_PER_SECOND, RealTimeClock, RtcError, UnixSeconds};

/// Low 32 bits of the current time. Reading it latches `TIME_HIGH`.
const GOLDFISH_TIME_LOW: usize = 0x00;
/// High 32 bits, as latched by the last `TIME_LOW` read.
const GOLDFISH_TIME_HIGH: usize = 0x04;

#[derive(Clone, Copy)]
pub(crate) struct GoldfishRtc {
    base: usize,
}

impl RealTimeClock for GoldfishRtc {
    const SOURCE: &'static str = "goldfish-rtc";

    fn read(&self) -> Result<UnixSeconds, RtcError> {
        // SAFETY: `base` is the identity-mapped device window of the
        // goldfish node the device tree describes, and both registers
        // are 32-bit reads at its start. The low half must be read
        // first: that read is what latches the high half.
        let (low, high) = unsafe {
            let low = ((self.base + GOLDFISH_TIME_LOW) as *const u32).read_volatile();
            let high = ((self.base + GOLDFISH_TIME_HIGH) as *const u32).read_volatile();
            (low, high)
        };
        let nanos = (u64::from(high) << 32) | u64::from(low);
        Ok(UnixSeconds::new(nanos / NANOS_PER_SECOND))
    }
}

/// Finds the platform's goldfish RTC.
///
/// A machine whose device tree describes none has no calendar to read,
/// and the caller leaves the kernel's wall clock unseeded.
pub(crate) fn discover(fdt: &Fdt<'_>) -> Option<GoldfishRtc> {
    let node = fdt.find_compatible(&["google,goldfish-rtc"])?;
    let region = node
        .reg()
        .and_then(|mut regions| regions.next())
        .unwrap_or_else(|| panic!("RISC-V goldfish RTC node has no usable reg property"));
    assert!(
        region.size.is_some_and(|size| size != 0),
        "RISC-V goldfish RTC reg property has zero size"
    );
    Some(GoldfishRtc {
        base: region.starting_address as usize,
    })
}
