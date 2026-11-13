//! Processors a platform's firmware adds to and removes from a running
//! machine.
//!
//! A hypervisor announces a hot-added or hot-removed vCPU through its
//! firmware interface: on x86 an ACPI general-purpose event whose method
//! `Notify`s the processor object, on arm64 an ACPI Generic Event Device
//! doing the same. The backend turns that into [`HotplugRequest`]s on the
//! possible set the kernel was sized for; the kernel answers each one by
//! bringing the processor online or taking it offline, and the backend
//! hands the answer back to firmware.
//!
//! # Concurrency contract
//!
//! One kernel task drives a [`ProcessorHotplug`] and is its only user, so
//! every method takes `&mut self` and none needs to be `Send`: the task is
//! pinned to the processor the backend routes the firmware's interrupt
//! to, and the interrupt handler reaches the task only through a signal
//! the backend owns. No method is called from interrupt context.

use alloc::vec::Vec;
use core::future::Future;

use crate::cpu::ProcessorId;

/// What firmware asks the kernel to do with a processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotplugAction {
    /// The processor was added: bring it online.
    Insert,
    /// Firmware wants the processor back: take it offline and release it.
    Eject,
}

/// One request firmware raised, on a processor of the possible set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotplugRequest {
    pub processor: ProcessorId,
    pub action: HotplugAction,
}

/// How the kernel answered a request, which the backend reports to
/// firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotplugOutcome {
    /// An inserted processor is online, or an ejected one is offline,
    /// stopped and released.
    Completed,
    /// The processor still runs work that cannot move, or is still
    /// coming up; it stays online and nothing was released.
    Busy,
    /// The kernel never releases this processor (the bootstrap
    /// processor).
    NotSupported,
}

/// The backend's side of processor hotplug.
pub trait ProcessorHotplug {
    /// Waits until firmware has raised at least one request, and returns
    /// every request raised since the previous call, in the order
    /// firmware raised them.
    fn requests(&mut self) -> impl Future<Output = Vec<HotplugRequest>> + '_;

    /// Releases `processor` to firmware for removal.
    ///
    /// The kernel calls this only once the processor is offline (or never
    /// ran), and keeps every other lifecycle transition out until it
    /// returns. The processor may still be on its way into its stop
    /// (`Cpu::stop_current`), so the implementation first waits until it
    /// executes nothing, then hands it to firmware; firmware may destroy
    /// it at once.
    fn eject(&mut self, processor: ProcessorId) -> impl Future<Output = ()> + '_;

    /// Tells firmware how the kernel answered `request`.
    fn report(&mut self, request: HotplugRequest, outcome: HotplugOutcome);
}
