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
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    dma: X86DmaPool,
    vector: u8,
    destination_apic_id: u32,
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
    helios_kernel::install_display_device(kernel, Arc::clone(&device));
    tracing::info!("virtio-gpu function={address} msix_vector={vector:#x}");
    VirtioDisplayDevice { device }
}
