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
#![no_std]

mod clock;
mod crb;
mod device;
mod tis;
mod transport;
mod window;

pub use clock::Clock;
pub use crb::Crb;
pub use device::{OpenError, SLOT_INDEX, Tpm};
pub use tis::Tis;
pub use transport::{Interface, InterfaceError, Transport, TransportError};
pub use window::{MmioWindow, RegisterWindow};
