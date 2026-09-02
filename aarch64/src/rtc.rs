//! PL031 real-time clock for the AArch64 backend.
//!
//! The virt board exposes an ARM PL031 next to the PL011 UART. Its data
//! register holds a free-running 32-bit counter of seconds since the
//! Unix epoch, which firmware loads from the host clock, so the kernel
//! reads one register once at boot and never touches the device again.
//!
//! Concurrency contract: discovery and the single read both happen on
//! the bootstrap processor before secondaries start, and the device is
//! never interrupt-driven, so nothing here is shared.

use fdt::Fdt;
use helios_hal::rtc::{RealTimeClock, RtcError, UnixSeconds};

use crate::LimineBootHandoff;

/// `RTCDR`, the current counter value.
const PL031_DATA: usize = 0x00;

#[derive(Clone, Copy)]
pub(crate) struct Pl031Rtc {
    base: usize,
}

impl RealTimeClock for Pl031Rtc {
    const SOURCE: &'static str = "pl031";

    fn read(&self) -> Result<UnixSeconds, RtcError> {
        // SAFETY: `base` is the mapped device window of the PL031 node
        // the device tree describes, and `RTCDR` is a 32-bit read-only
        // register at its start.
        let seconds = unsafe { ((self.base + PL031_DATA) as *const u32).read_volatile() };
        Ok(UnixSeconds::new(u64::from(seconds)))
    }
}

/// Finds the platform's PL031 and maps its register window.
///
/// A machine whose device tree describes no PL031 has no calendar to
/// read, and the caller leaves the kernel's wall clock unseeded.
pub(crate) fn discover(
    fdt: &Fdt<'_>,
    physical_memory_offset: usize,
    handoff: &LimineBootHandoff,
) -> Option<Pl031Rtc> {
    let node = fdt.all_nodes().find(|node| {
        node.compatible()
            .is_some_and(|compatible| compatible.all().any(|entry| entry == "arm,pl031"))
    })?;
    let region = node
        .raw_reg()
        .and_then(|mut regions| regions.next())
        .unwrap_or_else(|| panic!("AArch64 PL031 node has no usable reg property"));
    let size = crate::fdt_cells_to_usize(region.size, "AArch64 PL031 reg size");
    assert!(size != 0, "AArch64 PL031 reg property has zero size");
    let physical_base = crate::fdt_cells_to_usize(region.address, "AArch64 PL031 reg address");
    crate::map_mmio_page(physical_base, physical_memory_offset, handoff);
    Some(Pl031Rtc {
        base: crate::mmio_virtual_base(physical_base, physical_memory_offset),
    })
}
