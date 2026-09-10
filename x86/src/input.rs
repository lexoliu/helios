//! virtio-input over PCI for the x86 backend.
//!
//! A machine with a desktop presents several input functions at once —
//! a keyboard, a relative pointer, an absolute tablet — so every one of
//! them is brought up here and handed to the kernel, each on a message
//! of its own.
//!
//! Concurrency contract: the functions are discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards
//! each of them delivers its events on its own MSI-X vector, so the
//! kernel task that drains a device parks on a notification instead of
//! polling its ring.

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;

use helios_kernel::ExternalInterruptHandler;
use helios_virtio::{DeviceType, VirtioInputDevice, VirtioPciTransport};
use pci_types::PciAddress;

use crate::exceptions::INPUT_INTERRUPT_VECTORS;
use crate::iommu::X86DmaPool;
use crate::pci::PciRoot;

type X86VirtioInputDevice = VirtioInputDevice<VirtioPciTransport<X86DmaPool>>;

/// The interrupt route's view of one input function.
///
/// The kernel's reader holds the same `Arc`, which satisfies the input
/// contract through hal's shared-handle impl; this newtype exists only
/// to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioInputFunction {
    device: Arc<X86VirtioInputDevice>,
}

/// One input function the bootstrap processor brought up, together with
/// the IDT vector its messages are delivered on.
pub(crate) struct InputInterrupt {
    pub(crate) vector: u8,
    pub(crate) device: VirtioInputFunction,
}

impl ExternalInterruptHandler for VirtioInputFunction {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// Every PCI function that carries an input device.
pub(crate) fn discover(pci: &PciRoot) -> Vec<PciAddress> {
    let functions: Vec<PciAddress> = pci.find_virtio_functions(DeviceType::Input).collect();
    assert!(
        functions.len() <= INPUT_INTERRUPT_VECTORS.len(),
        "platform exposes {} input functions but only {} interrupt vectors exist",
        functions.len(),
        INPUT_INTERRUPT_VECTORS.len()
    );
    functions
}

/// Brings up every input function, hands them all to the kernel, and
/// publishes the service `helios:system/input` is served from.
///
/// One call for every function rather than one per function: the kernel
/// owns the machine's input devices as a set, because that is what a
/// program asking "what can I read?" is answered from.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    functions: &[(PciAddress, X86DmaPool)],
    destination_apic_id: u32,
    debug_state: &crate::debug_state::RuntimeState,
) -> Vec<InputInterrupt>
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let discovered: Vec<InputInterrupt> = functions
        .iter()
        .zip(INPUT_INTERRUPT_VECTORS)
        .map(|((address, dma), vector)| {
            let msix_vector = pci.bind_msix_vector(*address, vector, destination_apic_id);
            let device = helios_virtio::input_from_pci(
                &pci.access(),
                *address,
                pci,
                *dma,
                Some(msix_vector),
            )
            .unwrap_or_else(|error| {
                panic!("failed to initialize the virtio-input function at {address}: {error}")
            });
            tracing::info!("virtio-input function={address} msix_vector={vector:#x}");
            InputInterrupt {
                vector,
                device: VirtioInputFunction {
                    device: Arc::new(device),
                },
            }
        })
        .collect();
    let service = helios_kernel::install_input_devices(
        kernel,
        discovered
            .iter()
            .map(|installed| Arc::clone(&installed.device.device)),
    );
    debug_state.install_input_service(service);
    discovered
}
