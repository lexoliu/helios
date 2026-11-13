//! The ACPI machinery the bare-metal backends share, on top of the `acpi`
//! crate's table parser and AML interpreter.
//!
//! - [`namespace`]: loading the DSDT and SSDTs into one interpreter.
//! - [`notify`]: collecting the `Notify` operations AML executes.
//! - [`gpe`]: the FADT's general-purpose event blocks and the `_Exx` /
//!   `_Lxx` methods that serve them.
//! - [`processor`]: processor objects as hotplug targets: which processor
//!   a `Notify` names (`_MAT`), its release (`_EJ0`) and the status the
//!   kernel reports back (`_OST`).
//!
//! The library knows ACPI and the possible set's [`ProcessorId`]s; it
//! knows nothing of a backend's processor slots, interrupt controllers or
//! the kernel. A backend maps a processor's interrupt-controller identity
//! to its slot, routes the SCI or the event device's interrupt, and runs
//! the methods this library names from a task, never from the interrupt.
//!
//! [`ProcessorId`]: helios_hal::cpu::ProcessorId

#![no_std]

extern crate alloc;

pub mod gpe;
pub mod namespace;
pub mod notify;
pub mod processor;

pub use namespace::{Interpreter, NamespaceError, NoRegionHandler, load_namespace};
pub use notify::{Notification, NotifyRecorder};
