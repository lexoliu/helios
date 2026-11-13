//! A TPM 2.0 driver for measured boot and sealing.
//!
//! The crate drives a TPM through either of the two memory-mapped
//! interfaces the TCG PC Client Platform TPM Profile (PTP) defines, the
//! FIFO interface ([`Tis`]) and the Command Response Buffer ([`Crb`]),
//! at locality 0. Firmware names which one a machine has: the ACPI TPM2
//! table's start method or a device tree `compatible`, both of which
//! [`Interface`] understands. Commands are marshalled with
//! `tpm2-protocol`, and [`Tpm`] implements
//! [`helios_hal::measured_boot::MeasuredBoot`] on top of whichever
//! interface the caller chose.
//!
//! # Concurrency contract
//!
//! A TPM runs one command at a time. A [`Tpm`] and the interface inside
//! it are owned by one task, and every operation takes `&mut self`; the
//! handle is never shared between processors and holds no lock. No
//! interrupt is used: every wait polls a status register and yields to
//! the executor each time the device is not ready, so a slow TPM never
//! pins a processor. Each wait is bounded by the timeout the PTP sets
//! for that state and fails with the state's name when it passes.
//!
//! # Discovery
//!
//! [`AcpiTpm2`] reads the ACPI TPM2 table both ACPI-described backends
//! find the TPM through, and [`qualify`] runs the checks every backend
//! runs before it hands the kernel a TPM: the registers the chain
//! measured the kernel into, the owner authorization the sealed slot
//! needs, and the loader's [`EventLog`] recording the kernel image.
#![no_std]

mod acpi;
mod boot;
mod clock;
mod crb;
mod device;
mod event_log;
mod tis;
mod transport;
mod window;

pub use acpi::{AcpiTpm2, AcpiTpm2Error, AcpiTpm2Locality, PC_CLIENT_FIFO_LOCALITY_0};
pub use boot::{
    Discovered, Discovery, InterfaceDescription, KERNEL_IMAGE_REGISTERS, LoaderLog, QualifyError,
    Unqualified, discover, qualify,
};
pub use clock::Clock;
pub use crb::Crb;
pub use device::{OpenError, OwnerAuthorization, SLOT_INDEX, Tpm};
pub use event_log::{EV_IPL, Event, EventLog, EventLogError, LIMINE_KERNEL_IMAGE, LoaderEvent};
pub use tis::Tis;
pub use transport::{Interface, InterfaceError, Transport, TransportError};
pub use window::{LOCALITY_BYTES, MmioWindow, RegisterWindow};
