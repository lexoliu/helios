//! What the firmware says this machine is made of.
//!
//! Two conventions describe an AArch64 machine, and which one the kernel
//! gets is a property of the firmware it booted under rather than of the
//! hardware. QEMU's `virt` board with EDK2 publishes a flattened device
//! tree when ACPI is off and a full set of ACPI tables when it is on;
//! Arm server platforms publish ACPI only. Both are decoded here into
//! one [`PlatformDescription`], and every device bring-up path in this
//! backend reads that and nothing else — no module walks a device tree
//! or an ACPI table on its own.
//!
//! The order is fixed and is not a fallback chain: a device tree is used
//! whenever Limine hands one over, and ACPI otherwise. A machine that
//! offers both is described by its device tree, because that is the
//! richer description of the two on the boards that offer both; a
//! machine that offers neither cannot be brought up and says so.
//! Neither path ever fills in a gap in the other.
//!
//! Concurrency contract: the description is built once, on the
//! bootstrap processor, before any secondary is started, and is
//! read-only from then on. Nothing here takes a lock.

use core::fmt;

use arm_gic::{IntId, Trigger};
use thiserror::Error;

use crate::LimineBootHandoff;

mod acpi;
mod dt;

pub(crate) use acpi::AcpiError;

/// Slots the `virt` board and Arm server platforms expose for
/// transport-discovered virtio devices. QEMU's `virt` publishes exactly
/// 32; a platform that describes more is refused rather than silently
/// truncated, because a dropped slot is a device that never comes up.
const MAX_VIRTIO_MMIO_SLOTS: usize = 32;

/// Processors whose redistributor frame the description can name.
///
/// A machine that describes more is refused rather than described
/// partially: a processor whose redistributor the kernel cannot name
/// is one it cannot take an interrupt on.
const MAX_PROCESSORS: usize = 64;

/// A physical MMIO window, as the firmware describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MmioRegion {
    pub(crate) base: usize,
    pub(crate) size: usize,
}

/// A shared peripheral interrupt, as the platform routes it.
///
/// `number` is the SPI index the GIC driver wants, which is 32 below the
/// INTID that ACPI calls a global system interrupt vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SpiInterrupt {
    pub(crate) number: u32,
    pub(crate) trigger: Trigger,
}

impl SpiInterrupt {
    /// The GIC interrupt identifier this SPI raises.
    pub(crate) fn intid(self) -> IntId {
        IntId::spi(self.number)
    }
}

/// One `virtio-mmio` transport slot: its register window and the
/// interrupt it raises.
///
/// A slot is a place a device may be, not a device: on the `virt` board
/// most of the 32 slots are empty, and the caller probes the window for
/// the device type it wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VirtioMmioSlot {
    pub(crate) region: MmioRegion,
    pub(crate) interrupt: SpiInterrupt,
}

/// The platform's console UART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConsoleDescription {
    pub(crate) region: MmioRegion,
    /// The line the UART raises, when the platform describes one. The
    /// kernel polls the console rather than driving it from an
    /// interrupt, so this is recorded and not yet used.
    pub(crate) interrupt: Option<SpiInterrupt>,
}

/// Which redistributor frame belongs to which processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RedistributorAffinity {
    pub(crate) mpidr: u64,
    pub(crate) base: usize,
}

/// The GICv3 register layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GicDescription {
    pub(crate) distributor: MmioRegion,
    /// The redistributor discovery range: one frame pair per processor,
    /// laid out back to back with the stride the frames report.
    pub(crate) redistributor: MmioRegion,
    /// The processor each redistributor frame belongs to, when the
    /// firmware names them. ACPI does, through the MADT's GICC entries;
    /// a device tree gives only the range, and the driver matches each
    /// frame by the affinity it reports for itself. Both are correct —
    /// this is the firmware's own answer where there is one.
    pub(crate) redistributors: Slots<RedistributorAffinity, MAX_PROCESSORS>,
}

impl GicDescription {
    /// Fails unless the firmware named a redistributor frame for every
    /// processor the kernel is about to run on.
    ///
    /// The driver still matches frames by the affinity each one reports
    /// for itself, because redistributor order is implementation
    /// defined and the hardware is the authority on it. This check is
    /// about the description: a machine whose MADT lists fewer
    /// processors than the bootloader started has a redistributor
    /// discovery range too short for the frames the driver will walk,
    /// and reading past its end is a fault, not a missing device. A
    /// device tree names no frames at all, and nothing is checked.
    pub(crate) fn check_covers(&self, processor_mpidrs: impl Iterator<Item = u64>) {
        if self.redistributors.len() == 0 {
            return;
        }
        for mpidr in processor_mpidrs {
            assert!(
                self.redistributor_base(mpidr).is_some(),
                "the platform describes no GIC redistributor for MPIDR {mpidr:#x}"
            );
        }
    }

    /// The redistributor frame the firmware assigned to a processor,
    /// where the firmware assigns them.
    pub(crate) fn redistributor_base(&self, mpidr: u64) -> Option<usize> {
        self.redistributors
            .iter()
            .find(|entry| entry.mpidr & MPIDR_AFFINITY_MASK == mpidr & MPIDR_AFFINITY_MASK)
            .map(|entry| entry.base)
    }
}

/// Affinity bits of `MPIDR_EL1`: Aff0..Aff2 in the low word and Aff3 in
/// bits 32..40. The MADT reports a processor's MPIDR in the same shape,
/// but firmware is not required to zero the bits outside them.
const MPIDR_AFFINITY_MASK: u64 = 0x0000_00ff_00ff_ffff;

/// Where the description came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlatformSource {
    DeviceTree,
    Acpi,
}

impl fmt::Display for PlatformSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DeviceTree => "device-tree",
            Self::Acpi => "acpi",
        })
    }
}

/// The machine, as its firmware describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlatformDescription {
    pub(crate) source: PlatformSource,
    pub(crate) console: ConsoleDescription,
    pub(crate) gic: GicDescription,
    /// The real-time clock, when the platform has one. A machine
    /// without one leaves the kernel's wall clock reading as uptime.
    pub(crate) rtc: Option<MmioRegion>,
    pub(crate) virtio: Slots<VirtioMmioSlot, MAX_VIRTIO_MMIO_SLOTS>,
    /// The entropy the bootloader left behind, from the device tree's
    /// `/chosen/rng-seed`. ACPI has no equivalent property, so an
    /// ACPI-described machine starts from the processor's own random
    /// source and the entropy device alone.
    pub(crate) boot_entropy_seed: Option<&'static [u8]>,
}

/// A fixed-capacity list built once during bring-up.
///
/// The description is assembled before the kernel has any use for a
/// growable collection and is copied by value afterwards, so the
/// capacity is part of the type. Overflow is a platform this backend
/// cannot describe, not a resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slots<T, const N: usize> {
    entries: [Option<T>; N],
    len: usize,
}

impl<T: Copy, const N: usize> Slots<T, N> {
    const fn new() -> Self {
        Self {
            entries: [None; N],
            len: 0,
        }
    }

    fn push(&mut self, entry: T, what: &str) {
        assert!(
            self.len < N,
            "AArch64 platform describes more than {N} {what}, which this backend cannot address"
        );
        self.entries[self.len] = Some(entry);
        self.len += 1;
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.entries[..self.len]
            .iter()
            .map(|entry| entry.expect("platform slot below the length is populated"))
    }
}

impl<T: Copy, const N: usize> Default for Slots<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Why the platform could not be described.
#[derive(Debug, Error)]
pub(crate) enum PlatformError {
    #[error(
        "Limine handed over neither a device tree nor an ACPI RSDP; \
         the platform cannot be described"
    )]
    NoFirmwareTables,
    #[error("the device tree is malformed: {0}")]
    DeviceTree(&'static str),
    #[error("the device tree describes no {0}")]
    DeviceTreeMissing(&'static str),
    #[error(transparent)]
    Acpi(#[from] AcpiError),
}

/// The firmware tables Limine handed this kernel.
///
/// Held rather than re-derived because the console is needed before the
/// kernel has a heap and the rest of the description is needed after,
/// and re-parsing between the two would be two chances to disagree.
pub(crate) enum PlatformTables {
    DeviceTree(fdt::Fdt<'static>),
    Acpi(acpi::AcpiPlatformTables),
}

impl PlatformTables {
    /// Picks the firmware description: the device tree when Limine
    /// provides one, otherwise ACPI.
    pub(crate) fn discover(handoff: &LimineBootHandoff) -> Result<Self, PlatformError> {
        if let Some(dtb) = handoff.tables.device_tree_blob {
            return Ok(Self::DeviceTree(dt::parse(dtb)?));
        }
        let rsdp = handoff
            .tables
            .acpi_rsdp
            .ok_or(PlatformError::NoFirmwareTables)?;
        Ok(Self::Acpi(acpi::open(rsdp)?))
    }

    /// The console UART alone.
    ///
    /// Split out from the rest because the kernel wants a serial port
    /// before it has an allocator, and both firmware descriptions can
    /// answer this one question without allocating: a device tree walk
    /// borrows the blob in place, and SPCR is a fixed-layout table the
    /// `acpi` crate reads through a mapping.
    pub(crate) fn console(&self) -> Result<ConsoleDescription, PlatformError> {
        match self {
            Self::DeviceTree(fdt) => dt::console(fdt),
            Self::Acpi(tables) => Ok(acpi::console(tables)?),
        }
    }

    /// Everything else the backend needs to bring devices up.
    ///
    /// Runs after the kernel heap exists: the ACPI path has to
    /// interpret AML to find the virtio transports, and an AML
    /// interpreter allocates.
    pub(crate) fn describe(
        &self,
        console: ConsoleDescription,
    ) -> Result<PlatformDescription, PlatformError> {
        match self {
            Self::DeviceTree(fdt) => dt::describe(fdt, console),
            Self::Acpi(tables) => Ok(acpi::describe(tables, console)?),
        }
    }
}
