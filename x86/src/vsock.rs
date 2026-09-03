//! virtio-vsock over PCI for the x86 backend.
//!
//! Concurrency contract: the function is discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards the
//! kernel's vsock service owns it — one receive pump plus whichever
//! tasks are transmitting — and completions arrive on the device's MSI-X
//! vector, so every waiter parks on a notification rather than polling.

extern crate alloc;

use alloc::sync::Arc;

use helios_kernel::ExternalInterruptHandler;
use helios_virtio::{DeviceType, OffsetDmaPool, VirtioPciTransport, VirtioVsockDevice};
use pci_types::PciAddress;

use crate::pci::PciRoot;

type X86VirtioVsockDevice = VirtioVsockDevice<VirtioPciTransport<OffsetDmaPool>>;

/// The interrupt route's view of the platform's vsock function.
///
/// The kernel's vsock service holds the same `Arc`, which satisfies the
/// device contract through hal's shared-handle impl; this newtype exists
/// only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioVsockFunction {
    device: Arc<X86VirtioVsockDevice>,
}

impl ExternalInterruptHandler for VirtioVsockFunction {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// The PCI function that carries the platform's vsock device.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::Vsock)
}

/// Brings up the virtio-vsock function at `address` and publishes it as
/// the machine's host link.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install<WatchdogImpl>(
    cpu: &crate::X86Cpu,
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    physical_memory_offset: usize,
    vector: u8,
    destination_apic_id: u32,
    debug_state: &crate::debug_state::RuntimeState,
) -> VirtioVsockFunction
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::vsock_from_pci(
        &pci.access(),
        address,
        pci,
        OffsetDmaPool::new(physical_memory_offset),
        Some(msix_vector),
    )
    .unwrap_or_else(|error| {
        panic!("failed to initialize the virtio-vsock function at {address}: {error}")
    });
    let device = Arc::new(device);
    let service = helios_kernel::install_vsock_device(kernel, cpu, device.clone());
    let guest_cid = service.guest_cid();
    debug_state.install_vsock_service(helios_kernel::ComponentHostVsockService::from_service(
        service,
    ));
    tracing::info!(
        guest_cid,
        "virtio vsock online transport=pci function={address} msix_vector={vector:#x}"
    );
    VirtioVsockFunction { device }
}
