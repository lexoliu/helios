//! virtio-gpu over PCI for the x86 backend.
//!
//! Concurrency contract: the function is discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards its
//! completions and its display-change announcements arrive on the
//! device's MSI-X vector, delivered to that same processor, which is why
//! the kernel task that follows the topology is local to it.

extern crate alloc;

use alloc::sync::Arc;

use helios_kernel::ExternalInterruptHandler;
use helios_virtio::{DeviceType, VirtioGpuDevice, VirtioPciTransport};
use pci_types::PciAddress;

use crate::iommu::X86DmaPool;
use crate::pci::PciRoot;

type X86VirtioGpuDevice = VirtioGpuDevice<VirtioPciTransport<X86DmaPool>>;

#[derive(Clone)]
pub(crate) struct VirtioDisplayDevice {
    device: Arc<X86VirtioGpuDevice>,
}

impl ExternalInterruptHandler for VirtioDisplayDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// The PCI function that carries the platform's display engine.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::Gpu)
}

/// Brings up the virtio-gpu function at `address` and hands it to the
/// kernel.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install<WatchdogImpl>(
    cpu: &crate::X86Cpu,
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    dma: X86DmaPool,
    vector: u8,
    destination_apic_id: u32,
    debug_state: &crate::debug_state::RuntimeState,
) -> VirtioDisplayDevice
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::gpu_from_pci(&pci.access(), address, pci, dma, Some(msix_vector))
        .unwrap_or_else(|error| {
            panic!("failed to initialize the virtio-gpu function at {address}: {error}")
        });
    let device = Arc::new(device);
    let service = helios_kernel::install_display_device(kernel, cpu, Arc::clone(&device));
    debug_state.install_display_service(service);
    let service = helios_kernel::install_gpu3d_device(kernel, cpu, Arc::clone(&device));
    debug_state.install_gpu3d_service(service);
    tracing::info!("virtio-gpu function={address} msix_vector={vector:#x}");
    VirtioDisplayDevice { device }
}
