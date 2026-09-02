//! virtio-entropy over PCI for the x86 backend.
//!
//! Concurrency contract: the function is discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards the
//! kernel's reseed task is its only reader and completions arrive on the
//! device's MSI-X vector, so the reader parks on a notification instead
//! of polling the used ring.

extern crate alloc;

use alloc::sync::Arc;
use core::future::Future;

use helios_hal::io::IoError;
use helios_kernel::{ExternalInterruptHandler, HardwareEntropySource, RootEntropyHandle};
use helios_virtio::{DeviceType, VirtioPciTransport, VirtioRngDevice};
use pci_types::PciAddress;

use crate::iommu::X86DmaPool;
use crate::pci::PciRoot;

type X86VirtioRngDevice = VirtioRngDevice<VirtioPciTransport<X86DmaPool>>;

#[derive(Clone)]
pub(crate) struct VirtioEntropyDevice {
    device: Arc<X86VirtioRngDevice>,
}

impl ExternalInterruptHandler for VirtioEntropyDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

impl HardwareEntropySource for VirtioEntropyDevice {
    fn fill<'a>(
        &'a self,
        buffer: &'a mut [u8],
    ) -> impl Future<Output = Result<(), IoError>> + Send + 'a {
        self.device.fill(buffer)
    }
}

/// The PCI function that carries the platform's entropy device.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::Entropy)
}

/// Brings up the virtio-entropy function at `address` and hands it to
/// the kernel's reseed task.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    dma: X86DmaPool,
    vector: u8,
    destination_apic_id: u32,
    root: RootEntropyHandle,
) -> VirtioEntropyDevice
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::rng_from_pci(&pci.access(), address, pci, dma, Some(msix_vector))
        .unwrap_or_else(|error| {
            panic!("failed to initialize the virtio-rng function at {address}: {error}")
        });
    let device = VirtioEntropyDevice {
        device: Arc::new(device),
    };
    helios_kernel::install_entropy_device(kernel, root, device.clone());
    tracing::info!(
        "virtio entropy online transport=pci function={address} msix_vector={vector:#x}"
    );
    device
}
