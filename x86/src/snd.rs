//! virtio-snd over PCI for the x86 backend.
//!
//! Concurrency contract: the function is discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards its
//! completions and its event reports arrive on the device's MSI-X
//! vector, delivered to that same processor, which is why the kernel
//! task that drains its event ring is local to it.

extern crate alloc;

use alloc::sync::Arc;

use helios_kernel::ExternalInterruptHandler;
use helios_virtio::{DeviceType, VirtioPciTransport, VirtioSndDevice};
use pci_types::PciAddress;

use crate::iommu::X86DmaPool;
use crate::pci::PciRoot;

type X86VirtioSndDevice = VirtioSndDevice<VirtioPciTransport<X86DmaPool>>;

/// The interrupt route's view of the machine's sound device.
///
/// The kernel's event drain holds the same `Arc`, which satisfies the
/// playback contract through hal's shared-handle impl; this newtype
/// exists only to give the route an interrupt handler to dispatch to.
#[derive(Clone)]
pub(crate) struct VirtioSoundDevice {
    device: Arc<X86VirtioSndDevice>,
}

impl ExternalInterruptHandler for VirtioSoundDevice {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// The PCI function that carries the platform's sound device.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::Sound)
}

/// Brings up the virtio-snd function at `address` and hands it to the
/// kernel.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    dma: X86DmaPool,
    vector: u8,
    destination_apic_id: u32,
) -> VirtioSoundDevice
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::snd_from_pci(&pci.access(), address, pci, dma, Some(msix_vector))
        .unwrap_or_else(|error| {
            panic!("failed to initialize the virtio-snd function at {address}: {error}")
        });
    let device = Arc::new(device);
    helios_kernel::install_sound_device(kernel, Arc::clone(&device));
    tracing::info!("virtio-snd function={address} msix_vector={vector:#x}");
    VirtioSoundDevice { device }
}
