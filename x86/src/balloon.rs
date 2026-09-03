//! virtio-balloon over PCI for the x86 backend.
//!
//! Concurrency contract: the function is discovered and programmed on
//! the bootstrap processor before interrupts are enabled. Afterwards the
//! kernel's balloon tasks are its only users. The function delivers no
//! interrupt this backend can route — QEMU's `virtio-balloon-pci` has
//! no `vectors` property and so no MSI-X capability, and the backend
//! routes device interrupts through MSI-X only — so a task local to the
//! same processor reads the function's interrupt status on the kernel
//! timer and wakes the driver the way the interrupt would have.

extern crate alloc;

use alloc::sync::Arc;
use core::time::Duration;

use helios_hal::cpu::Cpu;
use helios_kernel::{BalloonHandle, Timer};
use helios_virtio::{DeviceType, OffsetDmaPool, VirtioBalloonDevice, VirtioPciTransport};
use pci_types::PciAddress;

use crate::pci::PciRoot;

type X86VirtioBalloonDevice = VirtioBalloonDevice<VirtioPciTransport<OffsetDmaPool>>;

/// How often the function's interrupt status is read.
///
/// The ISR status byte is read-to-clear and reports exactly what an
/// interrupt would have — a used buffer, a configuration change — so
/// reading it on a timer delivers every completion at most one interval
/// late, the same way the virtio-iommu's faults are collected. Balloon
/// traffic is memory-pressure management rather than a latency path;
/// the interval keeps the idle cost to one MMIO read per tick.
const INTERRUPT_POLL_INTERVAL: Duration = Duration::from_millis(10);

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
) -> BalloonHandle
where
    WatchdogImpl: helios_hal::watchdog::Watchdog + Clone,
{
    let device = helios_virtio::balloon_from_pci(
        &pci.access(),
        address,
        pci,
        OffsetDmaPool::new(physical_memory_offset),
        None,
    )
    .unwrap_or_else(|error| {
        panic!("failed to initialize the virtio-balloon function at {address}: {error}")
    });
    let device = Arc::new(device);
    let handle = helios_kernel::install_memory_balloon(kernel, device.clone());
    // Local, like the balloon tasks it wakes: `install_memory_balloon`
    // keeps those on the processor that brought the device up.
    kernel.spawn_local_detached(poll_interrupt_status(device, kernel.timer()));
    tracing::info!(
        "virtio balloon online transport=pci function={address} interrupt=polled \
         every {INTERRUPT_POLL_INTERVAL:?}"
    );
    handle
}

/// Stands in for the interrupt the function cannot deliver.
async fn poll_interrupt_status<CpuImpl: Cpu + Clone>(
    device: Arc<X86VirtioBalloonDevice>,
    timer: Timer<CpuImpl>,
) {
    loop {
        timer.sleep_for(INTERRUPT_POLL_INTERVAL).await;
        device.handle_interrupt();
    }
}
