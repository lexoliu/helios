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
//! A device-tree boot reads two trees. Devices and the console come from
//! the one Limine hands over; locality comes from the firmware's own,
//! which [`efi`] finds through the UEFI configuration table, because
//! Limine deletes every memory node from the copy it hands over.
//!
//! Concurrency contract: the description is built once, on the
//! bootstrap processor, before any secondary is started, and is
//! read-only from then on. Nothing here takes a lock.

use core::fmt;
use core::ptr::NonNull;

use arm_gic::{IntId, Trigger};
pub(crate) use helios_firmware::MPIDR_AFFINITY_MASK;
use helios_firmware::{DeviceTreeNuma, FirmwareNumaError, ProcessorIdentities};
use helios_hal::numa::NumaTopology;
use r_efi::efi::SystemTable;
use thiserror::Error;

use crate::LimineBootHandoff;

mod acpi;
mod dt;
mod efi;

pub(crate) use acpi::AcpiError;
use efi::{FirmwareTreeError, FirmwareTreeLocator};

/// Slots the `virt` board and Arm server platforms expose for
/// transport-discovered virtio devices. QEMU's `virt` publishes exactly
/// 32; a platform that describes more is refused rather than silently
/// truncated, because a dropped slot is a device that never comes up.
const MAX_VIRTIO_MMIO_SLOTS: usize = 32;

/// Devices the description can offer to a user-mode driver.
///
/// Bounded for the same reason as the virtio slots: the description is
/// built before the kernel has a growable collection, and a machine
/// with more grantable devices than this is one this backend cannot
/// describe rather than one it describes partially.
const MAX_GRANTABLE_DEVICES: usize = 8;

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

/// A device the kernel has no driver for, and can therefore hand to a
/// user-mode one.
///
/// "No driver for" is the whole definition: the console, the interrupt
/// controller, the real-time clock and every virtio transport are
/// driven by this backend, so they are never offered. What is left on a
/// device-tree machine is the hardware nobody in the kernel claims,
/// which is exactly the hardware a driver plugin exists to drive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GrantableDevice {
    /// The firmware's own name for the node, used as the grant's name so
    /// a driver asks for the device the machine describes rather than
    /// for a number this backend made up.
    pub(crate) name: &'static str,
    pub(crate) region: MmioRegion,
    pub(crate) interrupt: SpiInterrupt,
    /// Whether the device's own accesses are coherent with the
    /// processor caches, as the firmware declares it.
    pub(crate) coherent: bool,
}

/// The platform's console UART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConsoleDescription {
    pub(crate) region: MmioRegion,
    /// The line the UART raises. The debug console's readers and
    /// writers park on the interrupt it reports, so a description that
    /// cannot name one is refused rather than recorded.
    pub(crate) interrupt: SpiInterrupt,
}

/// The machine's TPM 2.0, as the firmware describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TpmDescription {
    /// Locality 0 of a TPM whose interface the firmware named.
    Described {
        base: u64,
        interface: helios_tpm::InterfaceDescription,
    },
    /// A TPM the ACPI TPM2 table describes through a start method this
    /// kernel does not drive.
    Unsupported { start_method: u32 },
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
    /// Hardware no kernel driver claims, offered to driver plugins.
    pub(crate) grantable: Slots<GrantableDevice, MAX_GRANTABLE_DEVICES>,
    /// The TPM 2.0, when the firmware describes one: the ACPI TPM2
    /// table, or a `tcg,tpm-tis-mmio` node.
    pub(crate) tpm: Option<TpmDescription>,
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
    #[error("the firmware's NUMA description is refused: {0}")]
    Numa(#[from] FirmwareNumaError),
    #[error("the firmware's own device tree cannot be read: {0}")]
    FirmwareTree(#[from] FirmwareTreeError),
    #[error(
        "the UEFI configuration table holds no device tree, and the tree Limine handed over \
         places processors on {nodes} NUMA nodes with its memory nodes deleted, so no memory \
         can be placed on them"
    )]
    NumaWithoutFirmwareTree { nodes: usize },
}

/// What the bootstrap processor does with the machine's NUMA description.
///
/// Each firmware path describes locality with a type of its own and the
/// consumer is generic over it, so [`PlatformTables::with_numa`] hands the
/// description over instead of returning one type for both paths.
pub(crate) trait NumaConsumer {
    type Output;

    fn consume<Numa: NumaTopology>(self, numa: &Numa) -> Self::Output;
}

/// The firmware tables Limine handed this kernel.
///
/// Held rather than re-derived because the console is needed before the
/// kernel has a heap and the rest of the description is needed after,
/// and re-parsing between the two would be two chances to disagree.
pub(crate) enum PlatformTables {
    DeviceTree(DeviceTreeTables),
    Acpi(acpi::AcpiPlatformTables),
}

/// The two trees a device-tree boot reads.
pub(crate) struct DeviceTreeTables {
    /// The tree Limine handed over: the firmware's, copied, with every
    /// `/memory` node deleted. The console and every device are read
    /// from it.
    handed: fdt::Fdt<'static>,
    /// Where the firmware's own tree is found. Locality is read from it
    /// and from nothing else, because memory nodes are what place memory
    /// on a node.
    firmware: FirmwareTreeLocator,
}

impl PlatformTables {
    /// Picks the firmware description: the device tree when Limine
    /// provides one, otherwise ACPI.
    ///
    /// `efi_system_table` is Limine's EFI system table response, at its
    /// direct-map address. Only a device-tree boot reads it, and only
    /// once [`Self::with_numa`] runs, so a failure to find the
    /// firmware's tree reaches the console.
    pub(crate) fn discover(
        handoff: &LimineBootHandoff,
        efi_system_table: Option<NonNull<SystemTable>>,
    ) -> Result<Self, PlatformError> {
        if let Some(dtb) = handoff.tables.device_tree_blob {
            return Ok(Self::DeviceTree(DeviceTreeTables {
                handed: dt::parse(dtb)?,
                firmware: FirmwareTreeLocator::new(
                    efi_system_table,
                    handoff.memory_map.entries,
                    crate::physical_memory_offset(),
                ),
            }));
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
            Self::DeviceTree(tables) => dt::console(&tables.handed),
            Self::Acpi(tables) => Ok(acpi::console(tables)?),
        }
    }

    /// The machine's NUMA description, handed to `consumer`.
    ///
    /// `identities` maps each processor to the affinity bits of its
    /// `MPIDR_EL1`. Neither path allocates, so this runs before the heap
    /// exists, which is when the bootstrap allocator needs the
    /// description: a device tree is read in place by
    /// [`DeviceTreeNuma`], and the SRAT, SLIT and MADT in place by
    /// [`acpi::numa`].
    ///
    /// A device-tree boot reads the firmware's own tree. A configuration
    /// table with no tree in it leaves only the one Limine handed over,
    /// which has no memory nodes: it describes the machine only when it
    /// carries no NUMA property at all, the one node `DeviceTreeNuma`
    /// then answers, and a tree that places processors on nodes is
    /// refused rather than read as one with no memory.
    pub(crate) fn with_numa<Identities, Consumer>(
        &self,
        identities: Identities,
        consumer: Consumer,
    ) -> Result<Consumer::Output, PlatformError>
    where
        Identities: ProcessorIdentities,
        Consumer: NumaConsumer,
    {
        match self {
            Self::DeviceTree(tables) => match tables.firmware.open()? {
                Some(firmware) => {
                    Ok(consumer.consume(&DeviceTreeNuma::new(&firmware, identities)?))
                }
                None => {
                    let numa = DeviceTreeNuma::new(&tables.handed, identities).map_err(
                        |error| match error {
                            FirmwareNumaError::NumaWithoutMemory { nodes } => {
                                PlatformError::NumaWithoutFirmwareTree { nodes }
                            }
                            error => PlatformError::Numa(error),
                        },
                    )?;
                    Ok(consumer.consume(&numa))
                }
            },
            Self::Acpi(tables) => Ok(consumer.consume(&acpi::numa(tables, identities)?)),
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
            Self::DeviceTree(tables) => dt::describe(&tables.handed, console),
            Self::Acpi(tables) => Ok(acpi::describe(tables, console)?),
        }
    }
}
