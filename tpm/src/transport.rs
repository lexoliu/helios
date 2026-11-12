//! What an interface does: carry one command frame to the TPM and its
//! response frame back.

use core::future::Future;
use core::time::Duration;

use thiserror::Error;

use crate::clock::Clock;
use crate::crb::Crb;
use crate::tis::Tis;
use crate::window::RegisterWindow;

/// Interface timeouts of the TCG PC Client Platform TPM Profile, as
/// Linux spells them for TPM 2.0 (`include/linux/tpm_command.h`,
/// `TPM2_TIMEOUT_A` to `TPM2_TIMEOUT_D`).
pub(crate) const TIMEOUT_A: Duration = Duration::from_millis(750);
pub(crate) const TIMEOUT_B: Duration = Duration::from_millis(4000);
pub(crate) const TIMEOUT_C: Duration = Duration::from_millis(200);
pub(crate) const TIMEOUT_D: Duration = Duration::from_millis(30);

/// How long one command may run: Linux's `TPM2_DURATION_DEFAULT`
/// (`include/linux/tpm_command.h`), the bound it applies to every
/// command it has no shorter figure for. None of the commands this
/// crate sends is one Linux allows longer.
pub(crate) const COMMAND_DURATION: Duration = Duration::from_millis(120_000);

/// The size of the header every command and response starts with:
/// a 2-byte tag, a 4-byte size and a 4-byte code (TPM 2.0 Part 1,
/// "Command/Response Structure").
pub(crate) const HEADER_LEN: usize = 10;

/// Why an interface could not complete an exchange.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum TransportError {
    #[error("the TPM did not reach {state} within {waited_ms} ms")]
    Timeout { state: &'static str, waited_ms: u64 },
    /// The interface broke its own protocol.
    #[error("the TPM interface reported {what}")]
    Protocol { what: &'static str },
    /// The CRB reported its fatal-error bit.
    #[error("the TPM reported a fatal error")]
    Fatal,
    #[error("the {len}-byte command does not fit the TPM's {capacity}-byte buffer")]
    CommandTooLarge { len: usize, capacity: usize },
    #[error(
        "the response header names {len} bytes, which is not between {HEADER_LEN} and {capacity}"
    )]
    ResponseSize { len: usize, capacity: usize },
    #[error(
        "the CRB {buffer} buffer at {address:#x} ({size} bytes) is outside the register window"
    )]
    BufferOutsideWindow {
        buffer: &'static str,
        address: u64,
        size: u32,
    },
    #[error("the register page identifies itself as interface type {found:#x}, not {expected}")]
    WrongInterface { expected: &'static str, found: u32 },
}

/// A route that carries TPM command frames.
pub trait Transport: Send {
    /// Sends `command`, one complete command frame, and writes the
    /// response frame into `response`, returning its length.
    fn transact<'a>(
        &'a mut self,
        command: &'a [u8],
        response: &'a mut [u8],
    ) -> impl Future<Output = Result<usize, TransportError>> + Send + 'a;
}

/// The two memory-mapped interfaces, for a caller that learns which one
/// the machine has only from firmware.
///
/// The choice is made at run time from a firmware table, and a platform
/// carries one measured-boot type, so the two interfaces meet in a sum
/// type rather than behind a trait object.
pub enum Interface<W, C> {
    Crb(Crb<W, C>),
    Tis(Tis<W, C>),
}

/// Why the interface firmware described could not be opened.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum InterfaceError {
    #[error("ACPI TPM2 start method {0} is neither 6 (memory-mapped FIFO) nor 7 (CRB)")]
    StartMethod(u32),
    #[error("the device tree node is not compatible with tcg,tpm-tis-mmio")]
    Compatible,
    #[error(transparent)]
    Transport(#[from] TransportError),
}

/// The two ACPI TPM2 table start methods that name a memory-mapped
/// interface: 6 for the FIFO interface, 7 for the Command Response
/// Buffer (TCG ACPI Specification, TPM2 table "Start Method"; QEMU
/// `include/hw/acpi/tpm.h` `TPM2_START_METHOD_MMIO` and
/// `TPM2_START_METHOD_CRB`).
pub(crate) const ACPI_START_METHOD_TIS: u32 = 6;
pub(crate) const ACPI_START_METHOD_CRB: u32 = 7;

/// The device tree `compatible` of the FIFO interface over MMIO, the
/// one QEMU writes for its `tpm-tis-device` (`hw/core/sysbus-fdt.c`).
const DT_COMPATIBLE_TIS: &str = "tcg,tpm-tis-mmio";

impl<W, C> Interface<W, C> {
    /// The interface's name, as the PTP calls it.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Crb(_) => "CRB",
            Self::Tis(_) => "FIFO",
        }
    }
}

impl<W: RegisterWindow, C: Clock> Interface<W, C> {
    /// The interface an ACPI TPM2 table's start method names.
    ///
    /// `window` is locality 0's register page and `physical_base` its
    /// physical address, which a CRB needs to find its buffers in it.
    pub async fn from_acpi_start_method(
        start_method: u32,
        window: W,
        physical_base: u64,
        clock: C,
    ) -> Result<Self, InterfaceError> {
        match start_method {
            ACPI_START_METHOD_TIS => Ok(Self::Tis(Tis::open(window, clock)?)),
            ACPI_START_METHOD_CRB => Ok(Self::Crb(Crb::open(window, physical_base, clock).await?)),
            other => Err(InterfaceError::StartMethod(other)),
        }
    }

    /// The interface a device tree node's `compatible` list names.
    pub fn from_device_tree<'a>(
        mut compatible: impl Iterator<Item = &'a str>,
        window: W,
        clock: C,
    ) -> Result<Self, InterfaceError> {
        if compatible.any(|entry| entry == DT_COMPATIBLE_TIS) {
            Ok(Self::Tis(Tis::open(window, clock)?))
        } else {
            Err(InterfaceError::Compatible)
        }
    }
}

impl<W: RegisterWindow, C: Clock> Transport for Interface<W, C> {
    async fn transact(
        &mut self,
        command: &[u8],
        response: &mut [u8],
    ) -> Result<usize, TransportError> {
        match self {
            Self::Crb(crb) => crb.transact(command, response).await,
            Self::Tis(tis) => tis.transact(command, response).await,
        }
    }
}

/// The response size a frame header names.
pub(crate) fn response_size(header: &[u8; HEADER_LEN]) -> usize {
    u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize
}
