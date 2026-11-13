//! Whether a TPM can carry this boot's measured state: the checks a
//! backend runs before it hands the kernel a TPM as its measured-boot
//! device.
//!
//! A TPM is handed over only when the chain that loaded the kernel
//! measured the kernel into it, because a sealed secret is only as good
//! as what was measured before the kernel ran: a kernel that measured
//! itself would prove nothing. So beyond the device being described and
//! reachable, three things must hold, in this order:
//!
//! 1. its SHA-256 bank keeps the [`KERNEL_IMAGE_REGISTERS`];
//! 2. the owner hierarchy accepts the empty authorization the sealed
//!    slot is defined and replaced with;
//! 3. the loader's event log records the loader measuring the kernel
//!    image ([`LIMINE_KERNEL_IMAGE`]).
//!
//! A check that does not hold is an [`Unqualified`] answer, which the
//! backend reports with a boot line and answers by handing over no
//! device. A device that fails while it is asked is a
//! [`QualifyError::Device`], which stops the boot: a TPM that is there
//! and broken is not one the kernel may quietly do without.

use core::fmt;

use helios_hal::boot::{MeasurementLog, MeasurementLogFormat};
use helios_hal::measured_boot::{MeasuredBootError, MeasurementRegister, MeasurementSet};
use thiserror::Error;

use crate::clock::Clock;
use crate::device::{OpenError, OwnerAuthorization, Tpm, transport_failed};
use crate::event_log::{EventLog, EventLogError, LIMINE_KERNEL_IMAGE};
use crate::tis::Tis;
use crate::transport::{Interface, InterfaceError, Transport};
use crate::window::RegisterWindow;

/// The registers the chain before the kernel measured the kernel image
/// and its loader into: UEFI firmware extends PCR 4 with every boot
/// application it starts, Limine among them (TCG PC Client Platform
/// Firmware Profile, "PCR[4]"; edk2 `DxeTpm2MeasureBootLib`), and Limine
/// extends PCR 9 with its configuration, the kernel and every module
/// (Limine `USAGE.md`, "Measured Boot").
pub const KERNEL_IMAGE_REGISTERS: MeasurementSet =
    MeasurementSet::of(&[MeasurementRegister::new(4), MeasurementRegister::new(9)]);

/// A check a TPM did not pass.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum Unqualified {
    #[error("the firmware describes the TPM through an interface this kernel does not drive: {0}")]
    Interface(InterfaceError),
    #[error("{0}")]
    Registers(OpenError),
    #[error(
        "the TPM's owner hierarchy has an authorization value set, and the sealed slot is \
         defined with the empty one"
    )]
    OwnerAuthorizationSet,
    #[error("the TPM's owner hierarchy is disabled, so the sealed slot cannot be defined")]
    OwnerHierarchyDisabled,
    #[error("the bootloader handed over no TPM event log")]
    NoEventLog,
    #[error("the TPM event log cannot be read: {0}")]
    EventLog(EventLogError),
    #[error(
        "the TPM event log records no EV_IPL event for the kernel image in PCR 9: the \
         bootloader did not measure the kernel (is `measured_boot: yes` in limine.conf?)"
    )]
    KernelImageNotMeasured,
}

/// Why a TPM was not handed over.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum QualifyError {
    #[error("{0}")]
    Unqualified(#[from] Unqualified),
    #[error("the TPM failed while it was checked: {0}")]
    Device(#[from] MeasuredBootError),
}

impl From<InterfaceError> for QualifyError {
    fn from(error: InterfaceError) -> Self {
        match error {
            // The register page answered for another interface than the
            // firmware named, or stopped answering: the device is there
            // and broken.
            InterfaceError::Transport(error) => {
                Self::Device(transport_failed("opening the interface", error))
            }
            InterfaceError::StartMethod(_) | InterfaceError::Compatible => {
                Self::Unqualified(Unqualified::Interface(error))
            }
        }
    }
}

/// What a backend's discovery found, for the boot line it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Discovery {
    /// The firmware describes no TPM.
    NotDescribed,
    /// The TPM was handed to the kernel.
    HandedOver { interface: &'static str, base: u64 },
    /// The firmware describes a TPM that failed a check; `base` is
    /// where its registers are, when the description got that far.
    Refused {
        base: Option<u64>,
        reason: Unqualified,
    },
}

/// The device a backend hands the kernel, if any, and the boot line that
/// says why.
pub type Discovered<W, C> = (Option<Tpm<Interface<W, C>>>, Discovery);

impl Discovery {
    /// Splits a qualification's outcome into the device to hand over
    /// and the boot line. A device that failed while it was checked is
    /// handed back to the caller, which stops the boot with it.
    fn of<W: RegisterWindow, C: Clock>(
        qualified: Result<Tpm<Interface<W, C>>, QualifyError>,
        base: u64,
    ) -> Result<Discovered<W, C>, MeasuredBootError> {
        match qualified {
            Ok(tpm) => {
                let interface = tpm.transport().name();
                Ok((Some(tpm), Self::HandedOver { interface, base }))
            }
            Err(QualifyError::Unqualified(reason)) => Ok((
                None,
                Self::Refused {
                    base: Some(base),
                    reason,
                },
            )),
            Err(QualifyError::Device(error)) => Err(error),
        }
    }

    /// Whether a described TPM was refused, which the boot line reports
    /// as a warning: the machine has a TPM and the signing seed still
    /// lasts one boot.
    pub const fn is_refusal(&self) -> bool {
        matches!(self, Self::Refused { .. })
    }
}

impl fmt::Display for Discovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotDescribed => formatter.write_str(
                "measured boot: the firmware describes no TPM; the signing seed lasts this boot",
            ),
            Self::HandedOver { interface, base } => write!(
                formatter,
                "measured boot: TPM 2.0 over the {interface} interface at {base:#x}, the kernel \
                 image measured into registers {:#x}",
                KERNEL_IMAGE_REGISTERS.bits()
            ),
            Self::Refused {
                base: Some(base),
                reason,
            } => write!(
                formatter,
                "measured boot: the TPM 2.0 at {base:#x} is not used, because {reason}; the \
                 signing seed lasts this boot"
            ),
            Self::Refused { base: None, reason } => write!(
                formatter,
                "measured boot: the described TPM is not used, because {reason}; the signing \
                 seed lasts this boot"
            ),
        }
    }
}

/// The loader's event log, as the boot handoff carries it.
#[derive(Clone, Copy, Debug)]
pub struct LoaderLog<'a> {
    pub format: MeasurementLogFormat,
    pub bytes: &'a [u8],
}

impl LoaderLog<'static> {
    /// The log a boot handoff names.
    ///
    /// # Safety
    ///
    /// `log.address..log.address + log.size` must be mapped, readable
    /// and left unchanged for the rest of the boot, as the bootloader's
    /// reclaimable memory is in a kernel that never reclaims it.
    pub unsafe fn mapped(log: &MeasurementLog) -> Self {
        Self {
            format: log.format,
            // SAFETY: the caller promised the range.
            bytes: unsafe { core::slice::from_raw_parts(log.address as *const u8, log.size) },
        }
    }
}

/// How the firmware names the interface a TPM's locality 0 answers
/// with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InterfaceDescription {
    /// The ACPI TPM2 table's start method.
    AcpiStartMethod(u32),
    /// A `tcg,tpm-tis-mmio` device-tree node: the FIFO interface.
    DeviceTreeFifo,
}

/// Opens the interface the firmware described at `base`, through the
/// register window a backend mapped there, runs every check of the
/// [module documentation](self), and splits the outcome into the device
/// to hand over and the boot line.
///
/// A device that failed while it was opened or checked is the error,
/// which stops the boot.
pub async fn discover<W: RegisterWindow, C: Clock>(
    interface: InterfaceDescription,
    window: W,
    base: u64,
    clock: C,
    log: Option<LoaderLog<'_>>,
) -> Result<Discovered<W, C>, MeasuredBootError> {
    let opened = match interface {
        InterfaceDescription::AcpiStartMethod(start_method) => {
            Interface::from_acpi_start_method(start_method, window, base, clock).await
        }
        InterfaceDescription::DeviceTreeFifo => Tis::open(window, clock)
            .map(Interface::Tis)
            .map_err(InterfaceError::from),
    };
    let qualified = match opened {
        Ok(interface) => qualify(interface, log).await,
        Err(error) => Err(error.into()),
    };
    Discovery::of(qualified, base)
}

/// Takes the TPM behind `transport` as this boot's measured-boot
/// device once every check of the [module documentation](self) holds.
pub async fn qualify<T: Transport>(
    transport: T,
    log: Option<LoaderLog<'_>>,
) -> Result<Tpm<T>, QualifyError> {
    let mut tpm =
        Tpm::open(transport, KERNEL_IMAGE_REGISTERS)
            .await
            .map_err(|error| match error {
                OpenError::Device(error) => QualifyError::Device(error),
                OpenError::NoSha256Bank | OpenError::RegistersMissing { .. } => {
                    QualifyError::Unqualified(Unqualified::Registers(error))
                }
            })?;
    match tpm.owner_authorization().await? {
        OwnerAuthorization::Accepted => {}
        OwnerAuthorization::Set => return Err(Unqualified::OwnerAuthorizationSet.into()),
        OwnerAuthorization::Disabled => return Err(Unqualified::OwnerHierarchyDisabled.into()),
    }
    let log = log.ok_or(Unqualified::NoEventLog)?;
    let measured = EventLog::parse(log.format, log.bytes)
        .and_then(|log| log.records(&LIMINE_KERNEL_IMAGE))
        .map_err(Unqualified::EventLog)?;
    if !measured {
        return Err(Unqualified::KernelImageNotMeasured.into());
    }
    Ok(tpm)
}
