//! virtio-balloon over PCI for the x86 backend.
//!
//! Concurrency contract: the function is discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards the
//! kernel's balloon tasks are its only users, and both its completions
//! and its configuration changes arrive on the device's MSI-X vector.

extern crate alloc;

use alloc::sync::Arc;

use helios_kernel::{BalloonHandle, ExternalInterruptHandler};
use helios_virtio::{DeviceType, OffsetDmaPool, VirtioBalloonDevice, VirtioPciTransport};
use pci_types::PciAddress;

use crate::pci::PciRoot;

type X86VirtioBalloonDevice = VirtioBalloonDevice<VirtioPciTransport<OffsetDmaPool>>;

/// The interrupt half of the balloon: what the IDT vector dispatches to.
///
/// The kernel's balloon tasks hold the device itself, so this carries
/// nothing but the same handle.
#[derive(Clone)]
pub(crate) struct VirtioBalloonInterrupt {
    device: Arc<X86VirtioBalloonDevice>,
}

impl ExternalInterruptHandler for VirtioBalloonInterrupt {
    fn handle_interrupt(&self) {
        self.device.handle_interrupt();
    }
}

/// The PCI function that carries the platform's memory balloon.
pub(crate) fn discover(pci: &PciRoot) -> Option<PciAddress> {
    pci.find_virtio_function(DeviceType::MemoryBalloon)
}

/// Brings up the virtio-balloon function at `address` and hands it to
/// the kernel's balloon tasks.
pub(crate) fn install<WatchdogImpl>(
    kernel: &helios_kernel::Kernel<crate::X86Cpu, WatchdogImpl>,
    pci: &PciRoot,
    address: PciAddress,
    physical_memory_offset: usize,
    vector: u8,
    destination_apic_id: u32,
) -> (VirtioBalloonInterrupt, BalloonHandle)
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let msix_vector = pci.bind_msix_vector(address, vector, destination_apic_id);
    let device = helios_virtio::balloon_from_pci(
        &pci.access(),
        address,
        pci,
        OffsetDmaPool::new(physical_memory_offset),
        Some(msix_vector),
    )
    .unwrap_or_else(|error| {
        panic!("failed to initialize the virtio-balloon function at {address}: {error}")
    });
    let device = Arc::new(device);
    let handle = helios_kernel::install_memory_balloon(kernel, device.clone());
    tracing::info!(
        "virtio balloon online transport=pci function={address} msix_vector={vector:#x}"
    );
    (VirtioBalloonInterrupt { device }, handle)
}
